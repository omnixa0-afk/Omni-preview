using System;
using System.Collections.Generic;
using System.Collections.ObjectModel;
using System.IO;
using System.Linq;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Omni.Windows.App.Core;
using Omni.Windows.App.Presentation;
using Windows.ApplicationModel.DataTransfer;
using Windows.Storage;

namespace Omni.Windows.App.Features.Groups;

/// One group conversation, shown in the Chats section in place of the 1:1 card when a group
/// row is selected: header, live call (tiles), the thread (own right, others left with names,
/// system lines centred), composer, and the group panel (rename, members/roles, add, leave).
/// Groups run on MLS — docs/decisions/group-crypto-mls.md; this control only renders and sends
/// commands, every rule (who may remove/rename) is enforced in the Rust core.
public sealed partial class GroupConversationControl : UserControl
{
    private RustBridge? bridge;
    private readonly ObservableCollection<GroupMessageItem> messages = new();
    private readonly ObservableCollection<GroupMemberItem> members = new();
    private readonly ObservableCollection<GroupInvitedItem> invited = new();
    private GroupSummary? group;
    private bool iAmAdmin;

    // The picture shown in the header (rebuilt only when the file changes).
    private string? shownAvatarPath;

    /// The window's image-file dialog and crop frame (the control has no window of its own): the path
    /// of the framed square, a temporary file this control deletes once the core took it; null = cancelled.
    public Func<Task<string?>>? PickImageFile { get; set; }
    // The only admin of a group with others in it: may not just leave (the core refuses), but name
    // another admin or delete the group for everyone.
    private bool iAmSoleAdmin;
    // Last rendered thread fingerprint (count + last id): the poll rebuilds only on change, so
    // scroll position and text selection survive the refresh tick.
    private string threadFingerprint = string.Empty;
    private string lastThreadMark = string.Empty;

    // The shell supplies the people picker (group id, group name → chosen 1:1 dialog keys, null = cancel).
    public Func<string, string, System.Threading.Tasks.Task<IReadOnlyList<string>?>>? PickPeople { get; set; }

    // Raised after the group changed in a way the chat list shows (rename, leave, new message).
    public event Action? GroupListChanged;
    // Raised after we left the group — the shell closes this conversation.
    public event Action<string>? GroupLeft;
    // Back to the chat list (narrow layout).
    public event Action? BackRequested;

    /// The narrow layout shows a back button (the 1:1 header has the same one).
    public bool ShowBackButton
    {
        get => GroupHeader.ShowBack;
        set => GroupHeader.ShowBack = value;
    }

    // The shared header's parts (Features/Shared/ConversationHeader).
    private Border GroupAvatarCircle => GroupHeader.Avatar.Circle;
    private TextBlock GroupAvatarTextBlock => GroupHeader.Avatar.Initials;
    private Image GroupAvatarPhoto => GroupHeader.Avatar.Photo;
    private TextBlock GroupNameTextBlock => GroupHeader.Title;
    private TextBlock GroupSubtitleTextBlock => GroupHeader.Subtitle;
    private TextBox GroupMessageInputTextBox => GroupComposer.Input;
    private Button SendGroupMessageButton => GroupComposer.SendButton;

    // The shell wires the native media pipeline: start group media for the group
    // dialog (CallVideoService.StartGroupMediaAsync) / stop it. Set by MainWindow.
    public Func<string, System.Threading.Tasks.Task>? StartGroupMedia { get; set; }
    public Func<System.Threading.Tasks.Task>? StopGroupMedia { get; set; }

    // Each participant tile's Image registers itself (Loaded) so the service renders that
    // sender's live video (or the local camera, for the "self" key) straight into it. Re-fires on
    // every tiles rebuild, replacing the previous target; the service clears the registry on StopAsync.
    public Action<string, Image>? RegisterTileVideo { get; set; }

    // Audio-first — the camera is an explicit opt-in even in a group call (same rule as 1:1).
    public Func<System.Threading.Tasks.Task>? ToggleGroupCamera { get; set; }
    public Func<bool>? IsGroupCameraActive { get; set; }

    // Screen sharing (broadcast screen instead of camera): Start shows the system picker then swaps
    // the video source to the screen; Stop returns to camera.
    public Func<bool>? IsGroupScreenSharing { get; set; }
    public Func<System.Threading.Tasks.Task>? StartGroupScreenShare { get; set; }
    public Func<System.Threading.Tasks.Task>? StopGroupScreenShare { get; set; }

    // Remote screen control (docs/decisions/remote-screen-control.md). Sharer: is the share a whole
    // monitor (only then may anyone control it), and the hook that readies the injector before a grant.
    // Viewer: start controlling `sharer` over its tile's Image / stop / whom we control now / a
    // rebuilt tile hands over its new Image.
    public Func<bool>? IsGroupScreenMonitorShared { get; set; }
    public Action? PrepareScreenControlGrant { get; set; }
    public Action<string, string, Image>? StartGroupScreenControl { get; set; }
    public Action? StopGroupScreenControl { get; set; }
    public Func<string?>? ControlledGroupSharer { get; set; }
    public Action<Image>? RebindGroupScreenControlSurface { get; set; }

    // Local mic mute for the group call (CallVideoService.SetLocalAudioMuted / IsLocalAudioMuted).
    public Action<bool>? SetGroupMuted { get; set; }
    public Func<bool>? IsGroupMuted { get; set; }

    // Given a tile's demux key, returns that endpoint's smoothed audio RMS (0..1) — lights the
    // tile's speaking ring.
    public Func<string, float>? ReadTileSpeakingLevel { get; set; }

    // How a member looks by account id: their photo (null — the letter stays), the name we gave them in
    // our contacts (null — the name the group knows), and a key that changes when either does.
    // MainWindow.GroupMemberPhotos.cs.
    public Func<string, MemberLook>? ResolveMember { get; set; }

    // The member list as the core gave it: lets the window take the non-contacts' names and photos
    // from their group cards (MainWindow.GroupMemberPhotos.cs).
    internal Action<IReadOnlyList<GroupMemberView>>? NoteMemberCards { get; set; }

    // Goes up when a member's photo changes: the thread and the call tiles redraw on the next tick
    // (the member rows see it in their signature).
    private int photosVersion;

    internal void MemberPhotosChanged()
    {
        photosVersion++;
        lastThreadMark = string.Empty;
    }

    private MemberLook LookOf(string? accountId) =>
        string.IsNullOrEmpty(accountId) || ResolveMember is null ? MemberLook.None : ResolveMember(accountId);

    // Above this RMS an endpoint counts as "speaking". Native already smooths (fast attack /
    // ~150 ms release), so the ring is stable; tuned against post-NS/AGC voice.
    private const float SpeakingLevelThreshold = 0.02f;

    private readonly DispatcherTimer speakingPollTimer = new()
    {
        Interval = TimeSpan.FromMilliseconds(120),
    };

    // Thread / members / call refresh while the conversation is on screen. The core raises
    // GroupChanged, but the shell has no event pump into controls yet — a 2 s poll is cheap
    // (two local SQLite reads) and matches the 1:1 thread's refresh cadence.
    private readonly DispatcherTimer refreshTimer = new()
    {
        Interval = TimeSpan.FromSeconds(2),
    };

    // The tiles currently on screen (focus tile + grid/filmstrip), so the poll can flip their
    // IsSpeaking in place without rebuilding (which would reset the video surfaces).
    private readonly List<GroupCallParticipantItem> liveTiles = new();

    // Remote mic/camera badges come from the call snapshot. Toggles are rare, so we re-read the
    // snapshot at a coarser cadence than the 120 ms speaking poll (~0.5 s latency is invisible).
    private const int SnapshotPollEveryTicks = 4;
    private int pollTicksSinceSnapshot;
    private const int StatusTextEveryTicks = 8;
    private int ticksSinceStatusText;

    private bool callActive;

    // The tile the user pinned to the big "focus" slot (demux key), or null for grid mode.
    private string? focusedTileKey;

    // The call's screen folded away (the chevron): the thread is back, the strip over it returns.
    private bool callFolded;

    // "вы пересылаете видео остальным" — the one topology fact a person can feel; set on join.
    private string callRelayingNote = string.Empty;

    // Tiles are 16:9 and as big as the stage lets them be; the strip under a pinned tile is small.
    private const double TileAspect = 16.0 / 9.0;
    private const double TileGap = 10; // the tile template's margin, both sides
    private const double MinTileWidth = 140;
    private const double MaxTileWidth = 960;
    private const double FilmstripTileWidth = 160;
    private const double FilmstripTileHeight = 90;

    /// Bubble fills come from a converter that doesn't follow a theme switch (see
    /// ChatBubbleBrushConverter); rebuilding the template runs it again.
    internal void RebuildMessageBubbles() => MainWindow.RebuildItemTemplate(GroupMessagesList);

    public GroupConversationControl()
    {
        InitializeComponent();
        GroupHeader.IdentityClicked += (_, _) => SetInfoOpen(!infoOpen);
        GroupComposer.ReplyDismissed += (_, _) => SetReply(null);
        Loaded += (_, _) =>
        {
            Shared.ColumnWidth.Details.Changed -= LayoutGroupInfoPanel;
            Shared.ColumnWidth.Details.Changed += LayoutGroupInfoPanel;
        };
        Unloaded += (_, _) => Shared.ColumnWidth.Details.Changed -= LayoutGroupInfoPanel;
        GroupMessagesList.SizeChanged += (_, _) =>
        {
            foreach (GroupMessageItem item in messages)
            {
                item.UpdateBubbleMaxWidth(BubbleMaxWidth());
            }
        };
        GroupHeader.BackRequested += (_, _) => BackRequested?.Invoke();
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(GroupHeader.IdentityButton, "Сведения о группе");
        GroupComposer.SendRequested += (_, _) => SendMessage();
        GroupComposer.AttachRequested += (_, _) =>
        {
            if (PickAnyFile?.Invoke() is { Length: > 0 } path)
            {
                SetDraft(path);
            }
        };
        GroupComposer.DraftRemoveRequested += (_, _) => ClearDraft();
        GroupComposer.VoiceRecorded += OnVoiceRecorded;
        GroupComposer.VideoNoteRecorded += OnVideoNoteRecorded;
        GroupComposer.VoiceFailed += (_, why) => SetStatus(why);
        GroupComposer.DraftAsPhotoChanged += (_, asPhoto) => draftAsPhoto = asPhoto;
        GroupMessageInputTextBox.TextChanged += (_, _) => UpdateSendEnabled();
        GroupMessagesList.ItemsSource = messages;
        // The composer takes files as the thread does (its text box takes drags of text itself).
        GroupComposer.AllowDrop = true;
        GroupComposer.AddHandler(DragOverEvent, new DragEventHandler(OnThreadDragOver), handledEventsToo: true);
        GroupComposer.AddHandler(DropEvent, new DragEventHandler(OnThreadDrop), handledEventsToo: true);
        // Pinned to the newest line until the reader scrolls up — and kept there when the thread
        // grows on its own (a photo decoding after the rows were laid out pushed the end out of view).
        GroupMessagesList.Loaded += (_, _) => WatchFeedScroll();
        GroupMembersListView.ItemsSource = members;
        GroupInvitedList.ItemsSource = invited;
        speakingPollTimer.Tick += OnSpeakingPollTick;
        refreshTimer.Tick += (_, _) => Refresh();
        SizeChanged += (_, _) => LayoutGroupInfoPanel();
        Unloaded += (_, _) =>
        {
            refreshTimer.Stop();
            speakingPollTimer.Stop();
        };
    }

    internal void Attach(RustBridge rustBridge) => bridge = rustBridge;

    /// «Код» on the group's page: the shell opens the group in the Workspaces section.
    public event Action<string>? WorkspaceRequested;

    private void OnWorkspaceTileClicked(object sender, RoutedEventArgs e)
    {
        if (group is not null)
        {
            WorkspaceRequested?.Invoke(group.GroupId);
        }
    }

    /// The group currently shown, or null.
    public string? CurrentGroupId => group?.GroupId;

    /// Show one group (from the chat list). Resets per-group view state.
    internal void ShowGroup(string groupId)
    {
        if (bridge is null)
        {
            return;
        }

        if (group?.GroupId != groupId)
        {
            focusedTileKey = null;
            threadFingerprint = string.Empty;
            SetReply(null);
            GroupComposer.CancelVoice();
            pinnedToBottom = true;
            lastThreadMark = string.Empty;
            messages.Clear();
            GroupMessageInputTextBox.Text = string.Empty;
            GroupManagePanel.Visibility = Visibility.Collapsed;
            SetStatus(null);
        }

        group = bridge.ListGroups().FirstOrDefault(g => g.GroupId == groupId);
        if (group is null)
        {
            SetStatus("Группа не найдена.");
            return;
        }

        callActive = TryGetCallActive(group.GroupId);
        Refresh();
        refreshTimer.Start();
    }

    /// Stop polling (the shell switched to a 1:1 chat or another section).
    internal void Hide()
    {
        refreshTimer.Stop();
        speakingPollTimer.Stop();
    }

    private void Refresh()
    {
        if (bridge is null || group is null)
        {
            return;
        }

        try
        {
            GroupSummary? fresh = bridge.ListGroups().FirstOrDefault(g => g.GroupId == group.GroupId);
            if (fresh is null)
            {
                // Gone (we left on another path, or were removed and the row was cleared).
                refreshTimer.Stop();
                GroupLeft?.Invoke(group.GroupId);
                return;
            }
            group = fresh;
            RefreshHeader();
            RefreshMembers();
            // The summary already says whether the thread moved (latest line + its time) — read the
            // whole thread only then, not every tick.
            // A file arriving moves nothing in the summary: while one is on its way, look every tick
            // (a downloaded photo kept its «not here yet» line, and a click on it did nothing).
            // An emoji, a pin or a deleted line in the middle move nothing there either: the actions' mark does.
            string threadMark = $"{fresh.LastActivityUnixMs}|{fresh.LastMessagePreview}|{bridge.GetMessageActions(fresh.DialogKey).Mark}";
            if (threadMark != lastThreadMark || threadFingerprint.Length == 0 || threadHasFilesInFlight)
            {
                lastThreadMark = threadMark;
                RefreshMessages();
            }
            RefreshCallState();
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось обновить группу: {ex.Message}");
        }
    }

    private void RefreshHeader()
    {
        if (group is null)
        {
            return;
        }

        GroupNameTextBlock.Text = group.Name;
        GroupAvatarTextBlock.Text = BuildInitials(group.Name);
        GroupAvatarCircle.Background = Omni.Windows.App.Presentation.AvatarPalette.BrushFor(group.GroupId);
        if (!string.Equals(shownAvatarPath, group.AvatarPath, StringComparison.Ordinal))
        {
            shownAvatarPath = group.AvatarPath;
            GroupAvatarPhoto.Source = Omni.Windows.App.Presentation.MainWindowPresentation.BuildAvatarImage(
                group.AvatarPath, 192, group.AvatarPath);
        }
        GroupAvatarPhoto.Visibility = GroupAvatarPhoto.Source is null ? Visibility.Collapsed : Visibility.Visible;
        GroupAvatarTextBlock.Visibility = GroupAvatarPhoto.Source is null ? Visibility.Visible : Visibility.Collapsed;
        // The group's page shows the same picture, big.
        GroupInfoAvatar.Initials.Text = GroupAvatarTextBlock.Text;
        GroupInfoAvatar.Circle.Background = GroupAvatarCircle.Background;
        GroupInfoAvatar.Photo.Source = GroupAvatarPhoto.Source;
        GroupInfoAvatar.Photo.Visibility = GroupAvatarPhoto.Visibility;
        GroupInfoAvatar.Initials.Visibility = GroupAvatarTextBlock.Visibility;
        GroupInfoNameTextBlock.Text = group.Name;
        bool active = group.MyStatus == "member";
        GroupSubtitleTextBlock.Text = active
            ? $"{group.ActiveMemberCount} {MembersWord(group.ActiveMemberCount)}"
            : group.MyStatus == "dissolved" ? "группа удалена" : "вы больше не участник";
        GroupInfoSubtitleTextBlock.Text = GroupSubtitleTextBlock.Text;
        GroupMessageInputTextBox.IsEnabled = active;
        groupActive = active;
        GroupComposer.ShowAttach = active;
        if (!active)
        {
            ClearDraft();
        }
        UpdateSendEnabled();
        AddGroupMembersButton.Visibility = active ? Visibility.Visible : Visibility.Collapsed;
        StartGroupCallButton.IsEnabled = active;
        GroupInfoCallTile.IsEnabled = active;
        GroupInfoInviteTile.Visibility = active ? Visibility.Visible : Visibility.Collapsed;
        GroupInfoWorkspaceTile.Visibility = active ? Visibility.Visible : Visibility.Collapsed;
        // Out of it (removed, or the group was deleted): what is left to do is delete the chat.
        LeaveGroupMenuItem.Text = active ? "Выйти из группы" : "Удалить чат";
        DeleteGroupMenuItem.Visibility = active && iAmAdmin ? Visibility.Visible : Visibility.Collapsed;
        LayoutActionTiles();
        // Don't overwrite what an admin is typing.
        if (GroupRenameTextBox.FocusState == FocusState.Unfocused)
        {
            GroupRenameTextBox.Text = group.Name;
        }
    }

    private void RefreshMembers()
    {
        if (bridge is null || group is null)
        {
            return;
        }

        IReadOnlyList<GroupMemberView> list = bridge.ListGroupMembers(group.GroupId);
        NoteMemberCards?.Invoke(list);
        iAmAdmin = list.Any(m => m.IsMe && m.Role == "admin");
        List<GroupMemberView> current = list.Where(m => m.Status == "member").ToList();
        iAmSoleAdmin = iAmAdmin && current.Count(m => m.Role == "admin") == 1 && current.Count > 1;
        bool manage = iAmAdmin && group.MyStatus == "member";
        DeleteGroupMenuItem.Visibility = manage ? Visibility.Visible : Visibility.Collapsed;
        GroupInfoManageTile.Visibility = manage ? Visibility.Visible : Visibility.Collapsed;
        if (!manage)
        {
            GroupManagePanel.Visibility = Visibility.Collapsed;
        }
        GroupClearAvatarButton.IsEnabled = group.AvatarPath is not null;
        GroupNewcomerHistoryCheckBox.IsChecked = group.NewcomerHistory;
        LayoutActionTiles();

        List<GroupMemberItem> fresh = list
            .Where(m => m.Status == "member")
            .OrderByDescending(m => m.IsMe)
            .ThenByDescending(m => m.Role == "admin")
            .ThenBy(m => m.DisplayName, StringComparer.CurrentCultureIgnoreCase)
            .Select(m => GroupMemberItem.From(m, iAmAdmin, LookOf(m.AccountId)))
            .ToList();
        GroupMembersHeaderTextBlock.Text = $"{fresh.Count} {MembersWord(fresh.Count)}";
        GroupComposer.SetMentionCandidates(list
            .Where(m => m.Status == "member" && !m.IsMe && m.AccountId.Length > 0)
            .DistinctBy(m => m.AccountId)
            .Select(m => new Shared.MentionCandidate(m.AccountId, m.DisplayName, string.Empty))
            .ToList());

        if (!fresh.Select(m => m.Signature).SequenceEqual(members.Select(m => m.Signature)))
        {
            members.Clear();
            foreach (GroupMemberItem item in fresh)
            {
                members.Add(item);
            }
        }

        List<GroupInvitedItem> pending = bridge.ListGroupSentInvites(group.GroupId)
            .Select(GroupInvitedItem.From)
            .ToList();
        if (!pending.Select(i => i.Signature).SequenceEqual(invited.Select(i => i.Signature)))
        {
            invited.Clear();
            foreach (GroupInvitedItem item in pending)
            {
                invited.Add(item);
            }
        }
        GroupInvitedHeaderTextBlock.Text = $"Приглашены · {invited.Count}";
        GroupInvitedHeaderTextBlock.Visibility = invited.Count > 0 ? Visibility.Visible : Visibility.Collapsed;

        // Alone in the group: the thread itself says so and offers the invite.
        bool alone = group.MyStatus == "member" && fresh.Count <= 1;
        GroupAloneNudge.Visibility = alone ? Visibility.Visible : Visibility.Collapsed;
        GroupAloneNudgeText.Text = invited.Count == 0
            ? "В группе пока только вы."
            : $"Ждём ответа: {string.Join(", ", invited.Select(i => i.Title))}.";
        GroupAloneNudgeButtonText.Text = invited.Count == 0 ? "Пригласить участников" : "Пригласить ещё";
    }

    private void RefreshMessages()
    {
        if (bridge is null || group is null)
        {
            return;
        }

        HookMessageActions();
        long readStarted = System.Diagnostics.Stopwatch.GetTimestamp();
        IReadOnlyList<GroupMessageView> list = bridge.ListGroupMessages(group.GroupId);
        MessageActionsView actions = bridge.GetMessageActions(group.DialogKey);
        threadHasFilesInFlight = list.Any(m => m.Attachment?.State is "preparing" or "requested" or "downloading" or "verifying" or "waiting_for_reconnect");
        // Files move on their own (a download's progress, a photo arriving): they are in it too.
        string files = string.Join(",", list
            .Where(m => m.Attachment is not null)
            .Select(m => $"{m.Attachment!.State}/{m.Attachment.BytesTransferred}/{m.Attachment.LocalCopyPath?.Length ?? 0}"));
        // So do reactions, on any line.
        string reactions = string.Join(",", list
            .Where(m => m.Reactions is { Count: > 0 })
            .Select(m => m.MessageId + string.Concat(m.Reactions!.Select(r => $"{r.Emoji}{r.Count}{r.Mine}"))));
        string fingerprint = list.Count == 0
            ? "0"
            : $"{list.Count}:{list[^1].MessageId}:{list[^1].Status}:{files}:{reactions}:{actions.Mark}:{members.Count}:{photosVersion}";
        if (fingerprint == threadFingerprint)
        {
            return;
        }

        bool firstFill = threadFingerprint.Length == 0;
        bool wasAtBottom = feedScroll is not { } shownScroll || shownScroll.VerticalOffset >= shownScroll.ScrollableHeight - 24;
        threadFingerprint = fingerprint;
        GroupPinnedBar.Show(actions.Pinned);
        // The author's colour comes from their account, as in the member list.
        Dictionary<string, string> accountByMember = members.ToDictionary(m => m.MemberPub, m => m.AccountId);
        for (int i = 0; i < list.Count; i++)
        {
            GroupMessageView view = list[i];
            GroupMessageView? next = i + 1 < list.Count ? list[i + 1] : null;
            GroupMessageView? previous = i > 0 ? list[i - 1] : null;
            // A run: one author's lines in a row on one day. Its first names them, its last has the avatar.
            string? dayHeader = previous is null || DayOf(previous) != DayOf(view)
                ? MainWindowPresentation.FormatDayHeader(view.CreatedAtUnixMs, DateTime.Today)
                : null;
            bool runHead = previous is null || dayHeader is not null || !SameRun(previous, view);
            bool runTail = next is null || DayOf(next) != DayOf(view) || !SameRun(view, next);
            string accountId = view.SenderPub is { } pub && accountByMember.TryGetValue(pub, out string? account)
                ? account
                : view.SenderPub ?? string.Empty;
            GroupMessageItem item = GroupMessageItem.From(view, accountId, runHead, runTail, dayHeader, BubbleMaxWidth(), LookOf(accountId));
            // Only the lines that changed are replaced: clearing the whole list under an open menu
            // or a pressed button took its target away (live 2026-10-11 the window went down).
            if (i < messages.Count)
            {
                if (messages[i].Signature != item.Signature)
                {
                    messages[i] = item;
                }
            }
            else
            {
                messages.Add(item);
            }
        }
        while (messages.Count > list.Count)
        {
            messages.RemoveAt(messages.Count - 1);
        }

        if (firstFill || wasAtBottom)
        {
            // Let the layout pass size the new rows, then pin to the newest message.
            pinnedToBottom = true;
            DispatcherQueue.TryEnqueue(Microsoft.UI.Dispatching.DispatcherQueuePriority.Low, GoToNewest);
        }

        if (!firstFill)
        {
            GroupListChanged?.Invoke();
        }
        else
        {
            MainWindow.TraceOpened(DispatcherQueue, $"group lines={list.Count}", readStarted);
        }
    }

    private ScrollViewer? feedScroll;
    private double feedOffset;

    // The list's own scroller, there once its template is. Pinned to the newest line until the reader
    // scrolls up, and kept there when the thread grows on its own: a photo decoding, and rows being
    // built below (their height is an estimate until then).
    private void WatchFeedScroll()
    {
        if (feedScroll is not null || FindScroller(GroupMessagesList) is not { } scroll)
        {
            return;
        }

        feedScroll = scroll;
        scroll.ViewChanged += (_, e) =>
        {
            if (e.IsIntermediate)
            {
                return;
            }

            if (scroll.VerticalOffset >= scroll.ScrollableHeight - 24)
            {
                pinnedToBottom = true;
            }
            else if (scroll.VerticalOffset < feedOffset - 1)
            {
                pinnedToBottom = false;
            }

            feedOffset = scroll.VerticalOffset;
        };
        scroll.RegisterPropertyChangedCallback(ScrollViewer.ExtentHeightProperty, (_, _) =>
        {
            if (pinnedToBottom)
            {
                scroll.ChangeView(null, scroll.ScrollableHeight, null, true);
            }
        });
    }

    private void GoToNewest()
    {
        if (messages.Count > 0)
        {
            GroupMessagesList.ScrollIntoView(messages[^1], ScrollIntoViewAlignment.Leading);
        }

        feedScroll?.ChangeView(null, feedScroll.ScrollableHeight, null, true);
    }

    private static ScrollViewer? FindScroller(DependencyObject parent)
    {
        for (int i = 0; i < Microsoft.UI.Xaml.Media.VisualTreeHelper.GetChildrenCount(parent); i++)
        {
            DependencyObject child = Microsoft.UI.Xaml.Media.VisualTreeHelper.GetChild(parent, i);
            if ((child as ScrollViewer ?? FindScroller(child)) is { } found)
            {
                return found;
            }
        }

        return null;
    }

    private static DateTime DayOf(GroupMessageView view)
        => DateTimeOffset.FromUnixTimeMilliseconds(view.CreatedAtUnixMs).ToLocalTime().Date;

    private static bool SameRun(GroupMessageView a, GroupMessageView b)
        => a.Kind != "system" && b.Kind != "system" && a.IsMine == b.IsMine && a.SenderPub == b.SenderPub;

    private double BubbleMaxWidth()
        => MainWindowPresentation.CalculateTranscriptBubbleMaxWidth(GroupMessagesList.ActualWidth, MainWindowPresentation.TranscriptRowChromeWidth);

    private void OnSendMessageClicked(object sender, RoutedEventArgs e) => SendMessage();

    /// Test channel: the composer's list after `@`.
    internal string MentionsForUiTest(string? choose) => GroupComposer.MentionsForUiTest(choose);

    // The line the next one answers, shown over the field until it goes or the cross is pressed.
    private Shared.MessageRow? replyTo;

    private void OnMessageReplyRequested(object? sender, Shared.MessageRow row) => SetReply(row);

    private void OnMessageReactionChosen(object? sender, string emoji)
    {
        if (sender is not Shared.MessageBubble { Row: { } row } || bridge is null)
        {
            return;
        }

        try
        {
            bridge.ReactToMessage(row.MessageId, emoji);
            RefreshMessages();
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось поставить реакцию: {ex.Message}");
        }
    }

    private void SetReply(Shared.MessageRow? row)
    {
        replyTo = row;
        if (row is null)
        {
            GroupComposer.ClearReply();
            return;
        }

        string text = row.Body.Length > 0 ? row.Body : row.AttachmentLabel;
        GroupComposer.ShowReply(row.IsOutgoing ? "Вы" : row.SenderName, text.Length > 0 ? text : "Сообщение");
    }

    /// A quote goes to its line.
    private void OnMessageQuoteOpened(object? sender, Shared.MessageRow row)
    {
        if (messages.FirstOrDefault(item => item.MessageId == row.QuotedMessageId) is { } target)
        {
            GroupMessagesList.ScrollIntoView(target, ScrollIntoViewAlignment.Leading);
        }
    }

    // ---- files ---------------------------------------------------------------------------------

    /// The window's open-file dialog (set by MainWindow).
    public Func<string?>? PickAnyFile { get; set; }

    /// A photo to show on the whole window: path, caption, file name.
    public event Action<string, string, string>? PhotoOpenRequested;

    /// «Сохранить как…» of a photo: path, file name.
    public event Action<string, string>? PhotoSaveRequested;

    /// A file's menu (open, download, revoke) at its plate: anchor, message id, the file.
    internal event Action<FrameworkElement, string, AttachmentRecord>? FileActionsRequested;

    private bool groupActive;
    private bool pinnedToBottom = true;
    private bool threadHasFilesInFlight;
    private string? draftPath;
    private string? draftName;
    private bool draftAsPhoto;

    /// A file dragged onto the thread becomes the draft, as with the paperclip (and as in the 1:1 chat).
    private void OnThreadDragOver(object sender, DragEventArgs e)
    {
        bool canAccept = groupActive && Shared.DroppedFiles.CanAttach(e.DataView);
        e.AcceptedOperation = canAccept ? DataPackageOperation.Copy : DataPackageOperation.None;
        if (canAccept)
        {
            e.DragUIOverride.Caption = "Отпустите, чтобы прикрепить файл";
        }
    }

    private async void OnThreadDrop(object sender, DragEventArgs e)
    {
        if (!groupActive || !Shared.DroppedFiles.CanAttach(e.DataView))
        {
            return;
        }

        try
        {
            AttachDropped(await Shared.DroppedFiles.ReadAsync(e.DataView));
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось прикрепить файл: {ex.Message}");
        }
    }

    /// Files dropped on the group (its thread, its composer, its row in the chat list): the first
    /// becomes the draft, as «Прикрепить» makes it.
    internal void AttachDropped(IReadOnlyList<StorageFile> files)
    {
        if (!groupActive)
        {
            return;
        }
        if (files.FirstOrDefault() is not { Path.Length: > 0 } file)
        {
            SetStatus("Можно перетащить файл или картинку, а не папку.");
            return;
        }

        SetDraft(file.Path);
        if (Shared.DroppedFiles.MoreThanOneNote(files) is { } note)
        {
            SetStatus(note);
        }
    }

    private void SetDraft(string path)
    {
        FileInfo file = new(path);
        if (!file.Exists)
        {
            SetStatus("Файл не найден.");
            return;
        }

        draftPath = file.FullName;
        draftName = file.Name;
        bool photoCandidate = MainWindowPresentation.CanSendAsPhoto(file.Name, file.Length);
        // A picture goes as a photo unless unticked (Telegram's «Сжать изображение»).
        draftAsPhoto = photoCandidate;
        GroupComposer.ShowDraft(file.FullName, file.Name, MainWindowPresentation.FormatFileSize(file.Length), photoCandidate, draftAsPhoto);
        UpdateSendEnabled();
    }

    private void ClearDraft()
    {
        draftPath = null;
        draftName = null;
        draftAsPhoto = false;
        GroupComposer.ClearDraft();
        UpdateSendEnabled();
    }

    /// Send is lit when there is something to send — the same rule as the 1:1 chat's.
    private void UpdateSendEnabled()
        => SendGroupMessageButton.IsEnabled = groupActive &&
            (draftPath is not null || !string.IsNullOrWhiteSpace(GroupMessageInputTextBox.Text));

    private GroupMessageItem? FindMessage(object sender)
        => sender is FrameworkElement { Tag: string messageId }
            ? messages.FirstOrDefault(m => m.MessageId == messageId)
            : null;

    private void OnPhotoClicked(object sender, RoutedEventArgs e)
    {
        if (FindMessage(sender) is not { Attachment: { } attachment } item)
        {
            return;
        }

        if (string.IsNullOrWhiteSpace(attachment.LocalCopyPath))
        {
            SetStatus("Фото ещё загружается.");
            return;
        }

        string author = item.IsOutgoing ? "Вы" : item.SenderName;
        PhotoOpenRequested?.Invoke(attachment.LocalCopyPath, $"{author} · {item.TimeText}", attachment.FileName);
    }

    private void OnPhotoSaveClicked(object sender, RoutedEventArgs e)
    {
        if (FindMessage(sender) is { Attachment: { LocalCopyPath: { Length: > 0 } path } attachment })
        {
            PhotoSaveRequested?.Invoke(path, attachment.FileName);
        }
        else
        {
            SetStatus("Фото ещё загружается.");
        }
    }

    /// Test channel: press the newest photo's button as a click would; say what the line holds.
    internal string TapLastPhotoForUiTest()
    {
        Button? last = null;
        void Walk(DependencyObject node)
        {
            for (int i = 0; i < Microsoft.UI.Xaml.Media.VisualTreeHelper.GetChildrenCount(node); i++)
            {
                DependencyObject child = Microsoft.UI.Xaml.Media.VisualTreeHelper.GetChild(node, i);
                if (child is Button { Visibility: Visibility.Visible } button &&
                    Microsoft.UI.Xaml.Automation.AutomationProperties.GetName(button) == "Фото")
                {
                    last = button;
                }
                Walk(child);
            }
        }
        Walk(GroupMessagesList);
        if (last is null)
        {
            return "error: no photo button";
        }
        GroupMessageItem? item = FindMessage(last);
        string state = $"tag={last.Tag} item={(item is null ? "none" : "found")} path={item?.Attachment?.LocalCopyPath ?? "null"} enabled={last.IsEnabled} hit={last.IsHitTestVisible}";
        new Microsoft.UI.Xaml.Automation.Peers.ButtonAutomationPeer(last).Invoke();
        return $"ok {state} status={GroupStatusTextBlock.Text}";
    }

    private void OnFileClicked(object sender, RoutedEventArgs e)
    {
        if (sender is FrameworkElement element && FindMessage(sender) is { Attachment: { } attachment } item)
        {
            FileActionsRequested?.Invoke(element, item.MessageId, attachment);
        }
    }


    /// A voice message from the composer: it answers what the text would have answered.
    private async void OnVoiceRecorded(object? sender, Features.Shared.VoiceRecording recording)
    {
        if (bridge is not { } core || group is not { } here)
        {
            return;
        }

        string? answering = replyTo?.MessageId;
        try
        {
            await Task.Run(() =>
            {
                string file = core.StageVoiceRecording(recording.Packets, recording.Waveform);
                core.SendGroupAttachment(here.GroupId, string.Empty, file, null, false, answering, asVoice: true);
            });
            SetReply(null);
            SetStatus(null);
            RefreshMessages();
        }
        catch (Exception ex)
        {
            SetStatus($"Голосовое не отправилось: {WorkspaceErrorText.ForCoreMessage(ex.Message)}");
        }
    }

    /// A video message from the composer: it answers what the text would have answered.
    private async void OnVideoNoteRecorded(object? sender, Features.Shared.VideoNoteRecording recording)
    {
        if (bridge is not { } core || group is not { } here)
        {
            return;
        }

        string? answering = replyTo?.MessageId;
        try
        {
            await Task.Run(() =>
            {
                string file = core.StageVideoNote(recording.Video, recording.Preview);
                core.SendGroupAttachment(here.GroupId, string.Empty, file, null, false, answering, asVideoNote: true);
            });
            SetReply(null);
            SetStatus(null);
            RefreshMessages();
        }
        catch (Exception ex)
        {
            SetStatus($"Видеосообщение не отправилось: {WorkspaceErrorText.ForCoreMessage(ex.Message)}");
        }
    }

    private void SendMessage()
    {
        if (bridge is null || group is null)
        {
            return;
        }

        string body = GroupMessageInputTextBox.Text?.Trim() ?? string.Empty;
        if (body.Length == 0 && draftPath is null)
        {
            return;
        }

        try
        {
            if (draftPath is { } path)
            {
                // The text goes as the file's caption, as in the 1:1 chat.
                bridge.SendGroupAttachment(group.GroupId, body, path, draftName, draftAsPhoto, replyTo?.MessageId);
                ClearDraft();
            }
            else
            {
                bridge.SendGroupText(group.GroupId, body, replyTo?.MessageId, GroupComposer.TakeMentions());
            }
            GroupMessageInputTextBox.Text = string.Empty;
            SetReply(null);
            SetStatus(null);
            RefreshMessages();
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось отправить: {WorkspaceErrorText.ForCoreMessage(ex.Message)}");
        }
    }

    // ---- group panel -------------------------------------------------------------------------

    // Below this width the group's page would squeeze the conversation's header off screen — it
    // covers the conversation instead.
    private bool infoOpen;

    /// The header (or «⋯») opens the group's page and closes it again, as in Telegram.
    private void OnGroupHeaderClicked(object sender, RoutedEventArgs e) => SetInfoOpen(!infoOpen);

    private void OnGroupInfoCloseClicked(object sender, RoutedEventArgs e) => SetInfoOpen(false);

    internal void SetInfoOpen(bool open)
    {
        infoOpen = open;
        if (!open)
        {
            GroupManagePanel.Visibility = Visibility.Collapsed;
        }
        LayoutGroupInfoPanel();
    }

    private Shared.RailSplitter? groupInfoEdge;

    private void LayoutGroupInfoPanel()
    {
        bool sideBySide = ActualWidth <= 0 || ActualWidth >= Shared.DetailsPanel.SideBySideFrom;
        GroupInfoPanel.Visibility = infoOpen ? Visibility.Visible : Visibility.Collapsed;
        GroupInfoColumn.Width = infoOpen && sideBySide ? new GridLength(Shared.DetailsPanel.Width) : new GridLength(0);
        Grid.SetColumn(GroupInfoPanel, sideBySide ? 1 : 0);
        Grid.SetColumnSpan(GroupInfoPanel, sideBySide ? 1 : 2);
        GroupInfoPanel.BorderThickness = sideBySide ? new Thickness(1, 0, 0, 0) : new Thickness(0);
        groupInfoEdge ??= Shared.DetailsPanel.AddEdge(GroupInfoPanel);
        groupInfoEdge.Visibility = sideBySide ? Visibility.Visible : Visibility.Collapsed;
    }

    /// The visible action tiles share the width equally (a member sees three, an admin four).
    private void LayoutActionTiles()
    {
        List<FrameworkElement> shown = GroupActionTilesGrid.Children
            .OfType<FrameworkElement>()
            .Where(tile => tile.Visibility == Visibility.Visible)
            .ToList();
        if (GroupActionTilesGrid.ColumnDefinitions.Count != shown.Count)
        {
            GroupActionTilesGrid.ColumnDefinitions.Clear();
            foreach (FrameworkElement _ in shown)
            {
                GroupActionTilesGrid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            }
        }
        for (int i = 0; i < shown.Count; i++)
        {
            Grid.SetColumn(shown[i], i);
        }
    }

    private void OnManageClicked(object sender, RoutedEventArgs e)
    {
        bool show = GroupManagePanel.Visibility != Visibility.Visible;
        GroupManagePanel.Visibility = show ? Visibility.Visible : Visibility.Collapsed;
        if (show && group is not null)
        {
            GroupRenameTextBox.Text = group.Name;
            GroupRenameTextBox.Focus(FocusState.Programmatic);
            GroupRenameTextBox.SelectAll();
        }
    }

    private void OnRenameKeyDown(object sender, KeyRoutedEventArgs e)
    {
        if (e.Key == global::Windows.System.VirtualKey.Enter)
        {
            e.Handled = true;
            OnRenameClicked(sender, e);
        }
    }

    private void OnRenameClicked(object sender, RoutedEventArgs e)
    {
        if (bridge is null || group is null)
        {
            return;
        }

        string name = GroupRenameTextBox.Text?.Trim() ?? string.Empty;
        if (name.Length == 0 || name == group.Name)
        {
            return;
        }

        try
        {
            bridge.RenameGroup(group.GroupId, name);
            SetStatus(null);
            Refresh();
            GroupListChanged?.Invoke();
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось переименовать: {ex.Message}");
        }
    }

    private async void OnAddMembersClicked(object sender, RoutedEventArgs e)
    {
        if (bridge is null || group is null || PickPeople is null)
        {
            return;
        }

        try
        {
            IReadOnlyList<string>? dialogKeys = await PickPeople(group.GroupId, group.Name);
            if (dialogKeys is null || dialogKeys.Count == 0)
            {
                return;
            }
            bridge.InviteToGroup(group.GroupId, dialogKeys);
            // The thread gets "Вы пригласили: …" and the panel lists them under "Приглашены".
            SetStatus(null);
            Refresh();
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось пригласить: {ex.Message}");
        }
    }

    private void OnCancelInviteClicked(object sender, RoutedEventArgs e)
    {
        if (bridge is null || group is null || sender is not FrameworkElement { Tag: string dialogKey })
        {
            return;
        }

        try
        {
            bridge.CancelGroupInvite(group.GroupId, dialogKey);
            SetStatus(null);
            Refresh();
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось отменить приглашение: {ex.Message}");
        }
    }

    private void OnMemberMenuClicked(object sender, RoutedEventArgs e)
    {
        if (sender is not FrameworkElement anchor || anchor.Tag is not string memberPub || group is null)
        {
            return;
        }

        GroupMemberItem? member = members.FirstOrDefault(m => m.MemberPub == memberPub);
        if (member is null)
        {
            return;
        }

        string groupId = group.GroupId;
        MenuFlyout menu = new();
        MenuFlyoutItem admin = new() { Text = member.IsAdmin ? "Снять права админа" : "Сделать админом" };
        admin.Click += (_, _) => RunMemberOp(() => bridge!.SetGroupAdmin(groupId, memberPub, !member.IsAdmin), "изменить роль");
        menu.Items.Add(admin);
        MenuFlyoutItem remove = new() { Text = "Удалить из группы" };
        remove.Click += (_, _) => RunMemberOp(() => bridge!.RemoveGroupMember(groupId, memberPub), "удалить участника");
        menu.Items.Add(remove);
        menu.ShowAt(anchor);
    }

    private void RunMemberOp(Action op, string what)
    {
        try
        {
            op();
            SetStatus(null);
            Refresh();
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось {what}: {ex.Message}");
        }
    }

    private async void OnLeaveGroupClicked(object sender, RoutedEventArgs e)
    {
        if (bridge is null || group is null)
        {
            return;
        }

        bool active = group.MyStatus == "member";
        if (active && iAmSoleAdmin)
        {
            await new ContentDialog
            {
                XamlRoot = XamlRoot,
                RequestedTheme = Presentation.MainWindowPresentation.ActiveTheme,
                Title = "Вы единственный админ",
                Content = "Чтобы выйти, сначала сделайте админом кого-то из участников (меню у его имени). Или удалите группу для всех.",
                CloseButtonText = "Понятно",
            }.ShowAsync();
            return;
        }

        string name = group.Name;
        bool confirmed = await ConfirmAsync(
            active ? "Выйти из группы?" : "Удалить чат?",
            active
                ? $"Вы выйдете из «{name}», и её переписка удалится с этого устройства. Вернуться можно только по новому приглашению.{ProjectsLeaveText(group.GroupId, forEveryone: false)}"
                : $"Переписка «{name}» удалится с этого устройства.",
            active ? "Выйти и удалить чат" : "Удалить");
        if (confirmed)
        {
            await RunAndCloseAsync(id => bridge.LeaveGroup(id), active ? "выйти из группы" : "удалить чат");
        }
    }

    private async void OnDeleteGroupClicked(object sender, RoutedEventArgs e)
    {
        if (bridge is null || group is null)
        {
            return;
        }

        bool confirmed = await ConfirmAsync(
            "Удалить группу для всех?",
            $"«{group.Name}» перестанет работать у всех участников: писать и звонить в неё больше нельзя. У вас чат удалится сразу, у остальных останется до тех пор, пока они его не удалят.{ProjectsLeaveText(group.GroupId, forEveryone: true)}",
            "Удалить для всех");
        if (confirmed)
        {
            await RunAndCloseAsync(id => bridge.DeleteGroupForEveryone(id), "удалить группу");
        }
    }

    /// What leaving (or deleting the group for everyone) takes of its projects, as a paragraph for the
    /// dialog; empty when the group has none. History goes, working folders stay (the core removes
    /// Omni's copy of a group we are no longer in — docs/decisions/workspace-files-storage.md).
    private string ProjectsLeaveText(string groupId, bool forEveryone)
    {
        GroupLeaveNotice notice;
        try
        {
            notice = bridge!.GetGroupLeaveNotice(groupId);
        }
        catch (Exception ex)
        {
            System.Diagnostics.Debug.WriteLine($"GetGroupLeaveNotice failed: {ex.Message}");
            return forEveryone
                ? "\n\nИстория версий проектов группы, если они есть, удалится у всех участников. Файлы в рабочих папках останутся."
                : "\n\nИстория версий проектов группы, если они есть, удалится с этого компьютера. Файлы в рабочих папках останутся.";
        }

        if (notice.Projects.Count == 0)
        {
            return string.Empty;
        }

        static string Names(List<string> names) => string.Join(", ", names.ConvertAll(n => $"«{n}»"));
        string where = forEveryone ? "у всех участников, в том числе у вас" : "с этого компьютера";
        string text = $"\n\nИстория версий {(notice.Projects.Count == 1 ? "проекта" : "проектов")} {Names(notice.Projects)} удалится {where}. Файлы в рабочих папках останутся.";
        if (!forEveryone && notice.OnlyCustodianOf.Count > 0)
        {
            text += notice.OnlyCustodianOf.Count == 1
                ? $"\n\nПолная история проекта {Names(notice.OnlyCustodianOf)} хранится только у вас: после выхода её не будет и у группы."
                : $"\n\nПолная история проектов {Names(notice.OnlyCustodianOf)} хранится только у вас: после выхода её не будет и у группы.";
        }

        return text;
    }

    private async Task<bool> ConfirmAsync(string title, string text, string primary)
    {
        ContentDialog confirm = new()
        {
            XamlRoot = XamlRoot,
            RequestedTheme = Presentation.MainWindowPresentation.ActiveTheme,
            Title = title,
            Content = text,
            PrimaryButtonText = primary,
            CloseButtonText = "Отмена",
            DefaultButton = ContentDialogButton.Close,
        };
        return await confirm.ShowAsync() == ContentDialogResult.Primary;
    }

    /// Leave the call first if we are in it, run the action, close the conversation.
    private async Task RunAndCloseAsync(Action<string> action, string what)
    {
        if (group is null)
        {
            return;
        }

        string groupId = group.GroupId;
        try
        {
            if (callActive)
            {
                await LeaveCallAsync();
            }
            action(groupId);
            refreshTimer.Stop();
            group = null;
            GroupLeft?.Invoke(groupId);
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось {what}: {ex.Message}");
        }
    }

    // ---- call ----------------------------------------------------------------------------------

    private void RefreshCallState()
    {
        if (group is null)
        {
            return;
        }

        callActive = TryGetCallActive(group.GroupId);
        GroupCallNotice? running = callActive ? null : TryGetRunningCall(group.GroupId);
        // An icon, as in a 1:1 chat; a call going on without us says so next to it.
        StartGroupCallButtonText.Text = running is not null ? $"Присоединиться · {running.ParticipantCount}" : string.Empty;
        StartGroupCallButtonText.Visibility = running is not null ? Visibility.Visible : Visibility.Collapsed;
        StartGroupCallButton.Padding = running is not null ? new Thickness(12, 0, 14, 0) : new Thickness(0);
        GroupInfoCallTileText.Text = running is not null ? "Присоединиться" : "Звонок";
        // In the call the way back is the strip over the thread; a second button said the same.
        StartGroupCallButton.Visibility = callActive ? Visibility.Collapsed : Visibility.Visible;
        GroupInfoCallTile.Visibility = StartGroupCallButton.Visibility;
        LayoutActionTiles();
        if (!callActive)
        {
            callFolded = false;
        }

        GroupCallStage.Visibility = callActive && !callFolded ? Visibility.Visible : Visibility.Collapsed;
        GroupCallStrip.Visibility = callActive && callFolded ? Visibility.Visible : Visibility.Collapsed;
        GroupCallStageTitle.Text = group.Name;
        RefreshCallTiles();
        RefreshCallStatusText();
    }

    /// "3 участника · 02:15" on the stage and the strip; the timer counts from the call's start.
    private void RefreshCallStatusText()
    {
        if (!callActive || lastCallSnapshot is not { } snapshot)
        {
            return;
        }

        int count = snapshot.ParticipantCount;
        string people = count <= 1 ? "ждём участников" : $"{count} {MembersWord(count)}";
        TimeSpan elapsed = TimeSpan.FromMilliseconds(
            Math.Max(0, DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() - snapshot.StartedAtUnixMs));
        string timer = elapsed.TotalHours >= 1 ? elapsed.ToString(@"h\:mm\:ss") : elapsed.ToString(@"mm\:ss");
        GroupCallBannerText.Text = $"{people} · {timer}{callRelayingNote}";
        GroupCallStripText.Text = $"Звонок · {people} · {timer}";
    }

    private void OnFoldCallClicked(object sender, RoutedEventArgs e)
    {
        callFolded = true;
        RefreshCallState();
    }

    private void OnExpandCallClicked(object sender, RoutedEventArgs e)
    {
        callFolded = false;
        RefreshCallState();
    }

    private async void OnLeaveCallClicked(object sender, RoutedEventArgs e) => await LeaveCallAsync();

    private async void OnChooseGroupAvatarClicked(object sender, RoutedEventArgs e)
    {
        if (bridge is null || group is null || PickImageFile is null)
        {
            return;
        }

        string? path;
        try
        {
            path = await PickImageFile();
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось открыть изображение: {ex.Message}");
            return;
        }

        if (path is { Length: > 0 })
        {
            RunGroupAvatarChange(path);
            try
            {
                File.Delete(path);
            }
            catch (IOException)
            {
                // A leftover in the window's temp folder, swept when the window starts.
            }
        }
    }

    private void OnClearGroupAvatarClicked(object sender, RoutedEventArgs e) => RunGroupAvatarChange(null);

    private void OnNewcomerHistoryClicked(object sender, RoutedEventArgs e)
    {
        if (bridge is null || group is null)
        {
            return;
        }

        bool on = GroupNewcomerHistoryCheckBox.IsChecked == true;
        try
        {
            bridge.SetGroupNewcomerHistory(group.GroupId, on);
            SetStatus(null);
            Refresh();
        }
        catch (Exception ex)
        {
            GroupNewcomerHistoryCheckBox.IsChecked = !on;
            SetStatus($"Не удалось изменить: {ex.Message}");
        }
    }

    private void RunGroupAvatarChange(string? path)
    {
        if (bridge is null || group is null)
        {
            return;
        }

        try
        {
            bridge.SetGroupAvatar(group.GroupId, path);
            SetStatus(null);
            Refresh();
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось изменить аватарку: {ex.Message}");
        }
    }

    /// The UI test channel's press of the chevron (fold) or the strip (unfold).
    internal void FoldCallForUiTest(bool fold)
    {
        callFolded = fold;
        RefreshCallState();
    }

    private void OnCallStageSizeChanged(object sender, SizeChangedEventArgs e) => LayoutCallTiles();

    private void OnSpeakingPollTick(object? sender, object e)
    {
        if (!callActive || ReadTileSpeakingLevel is null)
        {
            return;
        }

        // The call's timer, about once a second.
        if (++ticksSinceStatusText >= StatusTextEveryTicks)
        {
            ticksSinceStatusText = 0;
            RefreshCallStatusText();
        }

        bool selfMuted = IsGroupMuted?.Invoke() ?? false;
        bool selfCameraOff = !(IsGroupCameraActive?.Invoke() ?? false);

        // Self is authoritative from the local toggles (every tick); remote badges are pulled from
        // the snapshot on the coarser cadence — the members a peer broadcast last touched.
        IReadOnlyDictionary<string, GroupCallMember>? remoteState = null;
        if (++pollTicksSinceSnapshot >= SnapshotPollEveryTicks)
        {
            pollTicksSinceSnapshot = 0;
            remoteState = TryReadRemoteMediaState();
        }

        foreach (GroupCallParticipantItem tile in liveTiles)
        {
            if (tile.IsSelf)
            {
                tile.IsMuted = selfMuted;
                tile.IsCameraOff = selfCameraOff;
                tile.IsScreenSharing = IsGroupScreenSharing?.Invoke() ?? false;
            }
            else if (remoteState is not null
                && remoteState.TryGetValue(tile.MemberPub, out GroupCallMember? member))
            {
                tile.IsMuted = member.IsMuted;
                tile.IsCameraOff = member.IsCameraOff;
                tile.IsScreenSharing = member.IsScreenSharing;
                tile.LetsMeControl = member.LetsMeControl;
            }

            tile.IsControlling = !tile.IsSelf && ControlledGroupSharer?.Invoke() == tile.MemberPub;

            float level = ReadTileSpeakingLevel(tile.TileKey);
            // A muted endpoint never shows the speaking ring even if the live mic still hears you.
            tile.IsSpeaking = !tile.IsMuted && level >= SpeakingLevelThreshold;
        }

        if (remoteState is not null)
        {
            RefreshScreenControl();
        }
    }

    // --- remote screen control -------------------------------------------------------------------

    /// Keep both halves honest after each snapshot: stop controlling a sharer who withdrew, withdraw
    /// our own grant if what we share is no longer a whole monitor, and show the grant button only
    /// while a monitor is shared.
    private void RefreshScreenControl()
    {
        if (ControlledGroupSharer?.Invoke() is { } controlled
            && liveTiles.FirstOrDefault(t => t.MemberPub == controlled) is not { LetsMeControl: true })
        {
            StopGroupScreenControl?.Invoke();
        }

        bool monitorShared = (IsGroupScreenSharing?.Invoke() ?? false) && (IsGroupScreenMonitorShared?.Invoke() ?? false);
        ScreenControlGrant? grant = lastCallSnapshot?.MyScreenControl;
        if (grant is not null && !monitorShared && bridge is not null && group is not null)
        {
            try
            {
                bridge.SetGroupCallScreenControl(group.GroupId, everyone: false, Array.Empty<string>());
            }
            catch (Exception)
            {
                // The next snapshot shows whether it took; the core drops the grant with the share anyway.
            }
        }

        GroupScreenControlButton.Visibility = monitorShared ? Visibility.Visible : Visibility.Collapsed;
        GroupScreenControlButton.IsOn = grant is { Everyone: true } or { Members.Count: > 0 };
        GroupScreenControlButton.Caption = grant switch
        {
            { Everyone: true } => "Управляют все",
            { Members.Count: > 0 } g => $"Управляют: {g.Members!.Count}",
            _ => "Управление",
        };
    }

    /// The grant menu: "Всем", then everyone else in the call, ticked as the current grant says.
    private void OnGroupScreenControlFlyoutOpening(object? sender, object e)
    {
        GroupScreenControlFlyout.Items.Clear();
        if (bridge is null || group is null)
        {
            return;
        }

        GroupCallSnapshot? snapshot;
        try
        {
            snapshot = bridge.GetGroupCallSnapshot(group.GroupId);
        }
        catch (Exception)
        {
            return;
        }

        if (snapshot is null)
        {
            return;
        }

        ScreenControlGrant? grant = snapshot.MyScreenControl;
        HashSet<string> granted = new(grant?.Members ?? Array.Empty<string>(), StringComparer.Ordinal);
        ToggleMenuFlyoutItem everyone = new() { Text = "Всем", IsChecked = grant?.Everyone ?? false };
        everyone.Click += (_, _) => ApplyScreenControlGrant(everyone.IsChecked, Array.Empty<string>());
        GroupScreenControlFlyout.Items.Add(everyone);
        GroupScreenControlFlyout.Items.Add(new MenuFlyoutSeparator());
        Dictionary<string, string> names = members.ToDictionary(m => m.MemberPub, m => m.DisplayName, StringComparer.Ordinal);
        foreach (GroupCallMember member in snapshot.Members.Where(m => !m.IsSelf))
        {
            string name = names.TryGetValue(member.MemberPub, out string? known) && !string.IsNullOrWhiteSpace(known)
                ? known
                : ShortPub(member.MemberPub);
            ToggleMenuFlyoutItem item = new()
            {
                Text = name,
                IsChecked = (grant?.Everyone ?? false) || granted.Contains(member.MemberPub),
                IsEnabled = !(grant?.Everyone ?? false),
            };
            string memberPub = member.MemberPub;
            item.Click += (_, _) =>
            {
                if (item.IsChecked)
                {
                    granted.Add(memberPub);
                }
                else
                {
                    granted.Remove(memberPub);
                }

                ApplyScreenControlGrant(false, granted.ToList());
            };
            GroupScreenControlFlyout.Items.Add(item);
        }
    }

    private void ApplyScreenControlGrant(bool everyone, IReadOnlyList<string> grantees)
    {
        if (bridge is null || group is null)
        {
            return;
        }

        try
        {
            PrepareScreenControlGrant?.Invoke();
            bridge.SetGroupCallScreenControl(group.GroupId, everyone, grantees);
            SetStatus(everyone
                ? "Управлять вашим экраном могут все участники."
                : grantees.Count > 0 ? $"Управлять вашим экраном можно: {grantees.Count}." : "Управление экраном отключено.");
            lastCallSnapshot = bridge.GetGroupCallSnapshot(group.GroupId);
            RefreshScreenControl();
        }
        catch (Exception ex)
        {
            SetStatus($"Управление экраном: {ex.Message}");
        }
    }

    /// "Управлять" on a sharer's tile: its video Image becomes our input surface.
    private void OnTileControlClicked(object sender, RoutedEventArgs e)
    {
        if (sender is not Button { Tag: string sharer } button || group is null)
        {
            return;
        }

        if (ControlledGroupSharer?.Invoke() == sharer)
        {
            StopGroupScreenControl?.Invoke();
        }
        else if (button.Parent is Panel tile && tile.Children.OfType<Image>().FirstOrDefault() is { } image)
        {
            StartGroupScreenControl?.Invoke(group.DialogKey, sharer, image);
        }

        foreach (GroupCallParticipantItem item in liveTiles)
        {
            item.IsControlling = !item.IsSelf && ControlledGroupSharer?.Invoke() == item.MemberPub;
        }
    }

    // Per-member mic/camera state from the call snapshot, keyed by member pub. Null on any failure
    // (no active call / bridge hiccup) — the poll then leaves badges as they are.
    private IReadOnlyDictionary<string, GroupCallMember>? TryReadRemoteMediaState()
    {
        if (bridge is null || group is null)
        {
            return null;
        }

        try
        {
            GroupCallSnapshot? snapshot = bridge.GetGroupCallSnapshot(group.GroupId);
            lastCallSnapshot = snapshot;
            return snapshot?.Members.ToDictionary(m => m.MemberPub, m => m, StringComparer.Ordinal);
        }
        catch
        {
            return null;
        }
    }

    // Announce our own mic/camera state to the group call so remote members badge our tile.
    internal void BroadcastSelfMediaState()
    {
        if (bridge is null || group is null || !callActive)
        {
            return;
        }

        bool muted = IsGroupMuted?.Invoke() ?? false;
        bool cameraOff = !(IsGroupCameraActive?.Invoke() ?? false);
        bool screenSharing = IsGroupScreenSharing?.Invoke() ?? false;
        // What we tell the others is what our own buttons show: a camera or a share that changed
        // without a press (a capture that ended, the test channel) left the button lit or dark
        // against the truth (night pass 2026-10-13).
        RefreshGroupMuteButtonState();
        RefreshGroupCameraButtonState();
        try
        {
            bridge.SetGroupCallMediaState(group.GroupId, muted, cameraOff, screenSharing);
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось передать состояние микрофона/камеры: {ex.Message}");
        }
    }

    private async void OnStartCallClicked(object sender, RoutedEventArgs e)
    {
        if (callActive)
        {
            callFolded = false;
            RefreshCallState();
            return;
        }

        await StartOrJoinCallAsync();
    }

    /// Start a call in the shown group, or join the one going on there (the core decides: one call
    /// per group). Also the ring bar's "Присоединиться".
    public async System.Threading.Tasks.Task StartOrJoinCallAsync()
    {
        if (bridge is null || group is null || callActive)
        {
            return;
        }

        try
        {
            GroupCallSnapshot snapshot = bridge.StartGroupCall(group.GroupId);
            // Start the native media pipeline on the group dialog (send/receive ride the
            // peer-SFU path via core dialog routing).
            if (StartGroupMedia is not null)
            {
                await StartGroupMedia(snapshot.DialogKey);
            }
            callActive = true;
            // The status speaks of people, not of the media topology ("полная сетка", "каскад, лист
            // дерева" were implementation words on the user's screen). The one topology fact a person
            // can feel — their uplink carries others' video — stays, in plain words.
            callRelayingNote = snapshot.Topology != "full_mesh" && snapshot.IsForwarder
                ? " · вы пересылаете видео остальным"
                : string.Empty;
            callFolded = false;
            RefreshCallState();
            RefreshGroupCameraButtonState();
            RefreshGroupMuteButtonState();
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось начать звонок: {ex.Message}");
        }
    }

    internal async System.Threading.Tasks.Task LeaveCallAsync()
    {
        if (bridge is null || group is null)
        {
            return;
        }

        try
        {
            if (StopGroupMedia is not null)
            {
                await StopGroupMedia();
            }
            bridge.LeaveGroupCall(group.GroupId);
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось выйти из звонка: {ex.Message}");
        }
        callActive = false;
        focusedTileKey = null;
        ToggleGroupCameraButton.IsOn = false;
        ToggleGroupScreenShareButton.IsOn = false;
        RefreshCallState();
    }

    private async void OnToggleGroupCameraClicked(object sender, RoutedEventArgs e)
    {
        if (ToggleGroupCamera is null)
        {
            return;
        }

        ToggleGroupCameraButton.IsEnabled = false;
        try
        {
            await ToggleGroupCamera();
            BroadcastSelfMediaState();
        }
        finally
        {
            RefreshGroupCameraButtonState();
            ToggleGroupCameraButton.IsEnabled = true;
        }
    }

    private void RefreshGroupCameraButtonState()
    {
        bool active = IsGroupCameraActive?.Invoke() ?? false;
        ToggleGroupCameraButton.IsOn = active;
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(
            ToggleGroupCameraButton, active ? "Выключить камеру" : "Включить камеру");
        RefreshGroupScreenShareButtonState();
    }

    private async void OnToggleGroupScreenShareClicked(object sender, RoutedEventArgs e)
    {
        bool sharing = IsGroupScreenSharing?.Invoke() ?? false;
        Func<System.Threading.Tasks.Task>? action = sharing ? StopGroupScreenShare : StartGroupScreenShare;
        if (action is null)
        {
            return;
        }

        ToggleGroupScreenShareButton.IsEnabled = false;
        try
        {
            await action();
            BroadcastSelfMediaState();
        }
        catch (Exception ex)
        {
            SetStatus($"Демонстрация экрана: {ex.Message}");
        }
        finally
        {
            RefreshGroupScreenShareButtonState();
            RefreshGroupCameraButtonState();
            ToggleGroupScreenShareButton.IsEnabled = true;
        }
    }

    private void RefreshGroupScreenShareButtonState()
    {
        bool sharing = IsGroupScreenSharing?.Invoke() ?? false;
        ToggleGroupScreenShareButton.IsOn = sharing;
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(
            ToggleGroupScreenShareButton, sharing ? "Остановить демонстрацию экрана" : "Демонстрация экрана");
    }

    private void OnToggleGroupMuteClicked(object sender, RoutedEventArgs e)
    {
        if (SetGroupMuted is null)
        {
            return;
        }

        bool nextMuted = !(IsGroupMuted?.Invoke() ?? false);
        SetGroupMuted(nextMuted);
        RefreshGroupMuteButtonState();
        // Reflect it on the self tile immediately (the poll also does, but this avoids a ~120ms lag).
        foreach (GroupCallParticipantItem tile in liveTiles.Where(t => t.IsSelf))
        {
            tile.IsMuted = nextMuted;
        }
        BroadcastSelfMediaState();
    }

    /// The UI test channel's press of the mic button (docs/decisions/ui-test-channel.md).
    internal void ToggleMuteForUiTest() => OnToggleGroupMuteClicked(this, new RoutedEventArgs());

    private void RefreshGroupMuteButtonState()
    {
        bool muted = IsGroupMuted?.Invoke() ?? false;
        string glyph = ((char)(muted ? 0xEC54 : 0xE720)).ToString();
        string name = muted ? "Включить микрофон" : "Выключить микрофон";
        ToggleGroupMuteButton.IsOn = muted;
        ToggleGroupMuteIcon.Glyph = glyph;
        GroupCallStripMuteIcon.Glyph = glyph;
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(ToggleGroupMuteButton, name);
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(GroupCallStripMuteButton, name);
    }

    private void SetStatus(string? text)
    {
        GroupStatusTextBlock.Text = text ?? string.Empty;
        GroupStatusTextBlock.Visibility = string.IsNullOrEmpty(text) ? Visibility.Collapsed : Visibility.Visible;
    }

    /// Size the tiles to the stage: grid mode — the column count that gives the biggest 16:9 tiles
    /// for this many people in this space; focus mode — the pinned tile takes what the strip under
    /// it leaves. Also on every stage resize (the tiles' size is bound one-way).
    private void LayoutCallTiles()
    {
        double width = GroupCallTilesHost.ActualWidth;
        double height = GroupCallTilesHost.ActualHeight;
        if (liveTiles.Count == 0 || width <= 0 || height <= 0)
        {
            return;
        }

        GroupCallParticipantItem? focused = focusedTileKey is null
            ? null
            : liveTiles.FirstOrDefault(t => t.TileKey == focusedTileKey);
        if (focused is not null)
        {
            List<GroupCallParticipantItem> rest = liveTiles.Where(t => t != focused).ToList();
            double stripHeight = rest.Count > 0 ? FilmstripTileHeight + TileGap + 8 : 0;
            double focusWidth = Math.Clamp(
                Math.Min(width - TileGap, (height - stripHeight - TileGap) * TileAspect), MinTileWidth, MaxTileWidth * 1.5);
            SizeTiles(new[] { focused }, focusWidth, focusWidth / TileAspect);
            SizeTiles(rest, FilmstripTileWidth, FilmstripTileHeight);
            GroupCallTilesList.MaxWidth = double.PositiveInfinity;
            Grid.SetRow(GroupCallTilesList, 1);
            return;
        }

        int count = liveTiles.Count;
        double best = 0;
        int bestColumns = 1;
        for (int columns = 1; columns <= count; columns++)
        {
            int rows = (count + columns - 1) / columns;
            double tile = Math.Min(width / columns - TileGap, (height / rows - TileGap) * TileAspect);
            if (tile > best)
            {
                best = tile;
                bestColumns = columns;
            }
        }

        double tileWidth = Math.Clamp(best, MinTileWidth, MaxTileWidth);
        SizeTiles(liveTiles, tileWidth, tileWidth / TileAspect);
        // The wrap grid fills rows left to right; capping its width makes it break where we chose.
        // (+ slack for the list's own chrome, less than any tile, so no extra column fits.)
        GroupCallTilesList.MaxWidth = bestColumns * (tileWidth + TileGap) + 24;
        Grid.SetRow(GroupCallTilesList, 0);
    }

    private static void SizeTiles(IEnumerable<GroupCallParticipantItem> tiles, double width, double height)
    {
        foreach (GroupCallParticipantItem tile in tiles)
        {
            tile.TileWidth = width;
            tile.TileHeight = height;
        }
    }

    // One video tile per call participant (self first), from the call snapshot's members mapped to
    // display names. Rebuilt only when the set of participants or the pinned tile changes — a
    // rebuild re-registers the video surfaces, so the 2 s refresh must not churn it.
    private string tilesFingerprint = string.Empty;
    private GroupCallSnapshot? lastCallSnapshot;

    private void RefreshCallTiles()
    {
        if (bridge is null || group is null || !callActive)
        {
            ClearCallTiles();
            return;
        }

        try
        {
            GroupCallSnapshot? snapshot = bridge.GetGroupCallSnapshot(group.GroupId);
            if (snapshot is null)
            {
                ClearCallTiles();
                return;
            }

            string fingerprint = string.Join(",", snapshot.Members.Select(m => m.MemberPub).OrderBy(p => p, StringComparer.Ordinal))
                + "|" + focusedTileKey + "|" + photosVersion;
            if (fingerprint == tilesFingerprint)
            {
                return;
            }
            tilesFingerprint = fingerprint;

            Dictionary<string, string> names = members
                .ToDictionary(m => m.MemberPub, m => m.DisplayName, StringComparer.Ordinal);
            Dictionary<string, string> accounts = members
                .ToDictionary(m => m.MemberPub, m => m.AccountId, StringComparer.Ordinal);

            List<GroupCallParticipantItem> all = snapshot.Members
                .Select(m =>
                {
                    string name = names.TryGetValue(m.MemberPub, out string? resolved)
                        && !string.IsNullOrWhiteSpace(resolved)
                            ? resolved
                            : ShortPub(m.MemberPub);
                    // Our own tile says "Вы", as the thread does ("Вы: …").
                    return new GroupCallParticipantItem
                    {
                        MemberPub = m.MemberPub,
                        DisplayName = m.IsSelf ? "Вы" : name,
                        Initials = BuildInitials(name),
                        IsSelf = m.IsSelf,
                        AvatarSeed = accounts.TryGetValue(m.MemberPub, out string? account) ? account : m.MemberPub,
                        Picture = LookOf(account).Image,
                        // Our own tile is fed by the local camera ("self"); remote tiles by that
                        // sender's stream.
                        TileKey = m.IsSelf ? "self" : m.SenderTagHex.ToLowerInvariant(),
                        // Seed badges from the snapshot so a fresh tile shows the right state
                        // immediately; the poll then keeps them current.
                        IsMuted = m.IsMuted,
                        IsCameraOff = m.IsCameraOff,
                        IsScreenSharing = m.IsScreenSharing,
                    };
                })
                .OrderByDescending(t => t.IsSelf)
                .ToList();

            // A pinned participant who left the call drops back to grid mode.
            if (focusedTileKey is not null && all.All(t => t.TileKey != focusedTileKey))
            {
                focusedTileKey = null;
            }

            GroupCallParticipantItem? focused = focusedTileKey is null
                ? null
                : all.FirstOrDefault(t => t.TileKey == focusedTileKey);

            if (focused is not null)
            {
                // Focus mode: one big pinned tile + the rest as a filmstrip. Each demux key
                // appears in exactly ONE list, so single-Image-per-key registration holds.
                List<GroupCallParticipantItem> rest = all.Where(t => t.TileKey != focusedTileKey).ToList();
                GroupCallFocusList.ItemsSource = new[] { focused };
                GroupCallFocusList.Visibility = Visibility.Visible;
                GroupCallTilesList.ItemsSource = rest;
            }
            else
            {
                GroupCallFocusList.ItemsSource = null;
                GroupCallFocusList.Visibility = Visibility.Collapsed;
                GroupCallTilesList.ItemsSource = all;
            }

            liveTiles.Clear();
            liveTiles.AddRange(all);
            LayoutCallTiles();
            if (!speakingPollTimer.IsEnabled)
            {
                speakingPollTimer.Start();
            }
        }
        catch (Exception ex)
        {
            SetStatus($"Звонок: не удалось обновить участников: {ex.Message}");
            ClearCallTiles();
        }
    }

    private void ClearCallTiles()
    {
        speakingPollTimer.Stop();
        liveTiles.Clear();
        tilesFingerprint = string.Empty;
        GroupCallFocusList.ItemsSource = null;
        GroupCallFocusList.Visibility = Visibility.Collapsed;
        GroupCallTilesList.ItemsSource = null;
    }

    // Tap a tile to pin it to the big focus slot; tap the pinned tile again to return to the grid.
    private void OnCallTileClicked(object sender, ItemClickEventArgs e)
    {
        if (e.ClickedItem is not GroupCallParticipantItem item)
        {
            return;
        }

        focusedTileKey = focusedTileKey == item.TileKey ? null : item.TileKey;
        RefreshCallTiles();
    }

    // A tile's video Image has loaded — hand it to the media service keyed by the tile's demux key.
    private void OnTileVideoLoaded(object sender, RoutedEventArgs e)
    {
        if (sender is Image image && image.Tag is string tileKey && tileKey.Length > 0)
        {
            RegisterTileVideo?.Invoke(tileKey, image);
            if (ControlledGroupSharer?.Invoke() is { } controlled
                && liveTiles.Any(t => t.TileKey == tileKey && t.MemberPub == controlled))
            {
                RebindGroupScreenControlSurface?.Invoke(image);
            }
        }
    }

    // A call going on in the group that we're not in (the button then joins it).
    private GroupCallNotice? TryGetRunningCall(string groupId)
    {
        try
        {
            return bridge!.ListGroupCallInbox().FirstOrDefault(c => c.GroupId == groupId && !c.Joined);
        }
        catch
        {
            // Unknown → the button says "Звонок"; pressing it still joins a running call (core rule).
            return null;
        }
    }

    private bool TryGetCallActive(string groupId)
    {
        try
        {
            return bridge!.GetGroupCallSnapshot(groupId) is not null;
        }
        catch
        {
            // A snapshot read failing means "no call we can show" — the button offers to start one.
            return false;
        }
    }

    private static string ShortPub(string memberPub)
        => string.IsNullOrEmpty(memberPub) ? "?" : memberPub[..Math.Min(8, memberPub.Length)];

    // One letter, as the chat list, the contacts and a 1:1 header draw the same person or group
    // ("C" in the list was "CA" in the header).
    internal static string BuildInitials(string name) =>
        Omni.Windows.App.Presentation.MainWindowPresentation.BuildAvatarText(name);

    internal static string MembersWord(int n)
    {
        int mod100 = n % 100;
        int mod10 = n % 10;
        if (mod100 is >= 11 and <= 14)
        {
            return "участников";
        }
        return mod10 switch
        {
            1 => "участник",
            >= 2 and <= 4 => "участника",
            _ => "участников",
        };
    }
}

/// How a member looks in the group: photo, our name for them, and what identifies both.
public sealed record MemberLook(Microsoft.UI.Xaml.Media.ImageSource? Image, string Key, string? Name)
{
    public static readonly MemberLook None = new(null, string.Empty, null);
}

/// One line of the group thread.
public sealed class GroupMessageItem : Shared.MessageRow
{
    private GroupMessageItem(string messageId) : base(messageId) { }

    internal AttachmentRecord? Attachment { get; private init; }
    private string PictureKey { get; init; } = string.Empty;
    private string Seed { get; init; } = string.Empty;

    // What the row shows — a row is replaced only when this changes.
    internal string Signature => string.Join('|',
        MessageId, Body, SystemText, TimeText, ReactionsSignature, IsPinned, QuotedMessageId, QuoteText, QuoteAuthor, IsRunTail, ShowSender, DayHeaderText, SenderName, Peer?.AvatarText, PictureKey, Seed,
        Attachment?.State, Attachment?.BytesTransferred, Attachment?.LocalCopyPath, Attachment?.Presentation is not null);

    internal static GroupMessageItem From(
        GroupMessageView view, string accountId, bool runHead, bool runTail, string? dayHeader, double bubbleMaxWidth,
        MemberLook look)
    {
        string name = look.Name ?? (string.IsNullOrWhiteSpace(view.SenderName) ? "Участник" : view.SenderName);
        Chats.ChatTranscriptPeer peer = new();
        peer.Set(GroupConversationControl.BuildInitials(look.Name ?? (string.IsNullOrWhiteSpace(view.SenderName) ? "?" : view.SenderName)), look.Image, accountId);
        GroupMessageItem item = new(view.MessageId)
        {
            Attachment = view.Attachment,
            PictureKey = look.Key,
            Seed = accountId,
            Peer = peer,
            SenderName = name,
            IsOutgoing = view.IsMine,
            ShowSender = runHead,
            IsRunTail = runTail,
            TimeText = MainWindowPresentation.FormatBubbleTime(view.CreatedAtUnixMs),
        };
        if (view.Kind == "system")
        {
            item.SystemText = view.Body;
        }
        else
        {
            item.Body = view.Body;
        }
        item.SetDayHeader(dayHeader);
        item.UpdateBubbleMaxWidth(bubbleMaxWidth);
        item.ApplyReply(view.Reply, "Участник");
        item.ApplyReactions(view.Reactions);
        item.IsPinned = view.Pinned;
        item.ApplyAttachment(
            view.Attachment,
            view.Attachment is null ? string.Empty : MainWindowPresentation.BuildGroupAttachmentStatusText(view.Attachment, view.IsMine));
        return item;
    }
}

/// One member row in the group panel.
public sealed class GroupMemberItem
{
    public string MemberPub { get; init; } = string.Empty;
    public string DisplayName { get; init; } = string.Empty;
    public string Title { get; init; } = string.Empty;
    public string Initials { get; init; } = string.Empty;
    public bool IsAdmin { get; init; }
    public bool CanManage { get; init; }
    public string AccountId { get; init; } = string.Empty;
    public Microsoft.UI.Xaml.Media.ImageSource? Picture { get; init; }
    private string PictureKey { get; init; } = string.Empty;

    // The member's colour everywhere (AvatarPalette); UI thread only.
    public Microsoft.UI.Xaml.Media.Brush AvatarBrush => Omni.Windows.App.Presentation.AvatarPalette.BrushFor(AccountId);

    public Visibility AdminVisibility => IsAdmin ? Visibility.Visible : Visibility.Collapsed;
    public Visibility MenuVisibility => CanManage ? Visibility.Visible : Visibility.Collapsed;

    // What the row shows — the list is rebuilt only when this changes.
    internal string Signature => $"{MemberPub}|{Title}|{IsAdmin}|{CanManage}|{PictureKey}";

    internal static GroupMemberItem From(
        GroupMemberView m, bool iAmAdmin, MemberLook look)
    {
        // Our name for a contact first: the same person was «Laptop» in the chat list and
        // «LaptopStand1» in the group (owner, 2026-10-13).
        string name = look.Name ?? (string.IsNullOrWhiteSpace(m.DisplayName) ? m.MemberPub[..Math.Min(8, m.MemberPub.Length)] : m.DisplayName);
        bool admin = m.Role == "admin";
        return new GroupMemberItem
        {
            MemberPub = m.MemberPub,
            AccountId = m.AccountId,
            Picture = look.Image,
            PictureKey = look.Key,
            DisplayName = name,
            Title = m.IsMe ? $"{name} (вы)" : name,
            Initials = GroupConversationControl.BuildInitials(name),
            IsAdmin = admin,
            // Only admins act on others; the core enforces it regardless (checked before merge).
            CanManage = iAmAdmin && !m.IsMe,
        };
    }
}

/// One invitation we sent and nobody answered yet (panel section "Приглашены").
public sealed class GroupInvitedItem
{
    public string DialogKey { get; init; } = string.Empty;
    public string Title { get; init; } = string.Empty;
    public string Detail { get; init; } = string.Empty;
    public string Initials { get; init; } = string.Empty;

    internal string Signature => $"{DialogKey}|{Title}|{Detail}";

    internal static GroupInvitedItem From(GroupSentInvite invite) => new()
    {
        DialogKey = invite.DialogKey,
        Title = invite.DisplayName,
        Detail = $"ждём ответа · с {Presentation.MainWindowPresentation.FormatMessageTime(invite.SentAtUnixMs)}",
        Initials = GroupConversationControl.BuildInitials(invite.DisplayName),
    };
}

/// One participant tile in a live group call. Shows identity (initials + name + self badge)
/// under the sender's live video.
public sealed class GroupCallParticipantItem : System.ComponentModel.INotifyPropertyChanged
{
    public string MemberPub { get; init; } = string.Empty;
    public string DisplayName { get; init; } = string.Empty;
    public string Initials { get; init; } = string.Empty;
    public bool IsSelf { get; init; }

    /// The video-demux key this tile's Image registers under with the media service:
    /// "self" (local camera) for our own tile, the sender's lowercase 8-byte tag hex otherwise.
    public string TileKey { get; init; } = string.Empty;

    /// The member's account id: their avatar colour is the one they have everywhere (AvatarPalette).
    public string AvatarSeed { get; init; } = string.Empty;

    public Microsoft.UI.Xaml.Media.ImageSource? Picture { get; init; }

    public Microsoft.UI.Xaml.Media.Brush AvatarBrush => Omni.Windows.App.Presentation.AvatarPalette.BrushFor(AvatarSeed);

    // Set by RefreshCallTiles before the tile is bound. x:Bind is OneTime and items are rebuilt
    // on each tiles change, so a plain settable value is enough.
    private double tileWidth = 150;
    public double TileWidth
    {
        get => tileWidth;
        set
        {
            if (tileWidth != value)
            {
                tileWidth = value;
                Raise(nameof(TileWidth));
            }
        }
    }

    private double tileHeight = 98;
    public double TileHeight
    {
        get => tileHeight;
        set
        {
            if (tileHeight != value)
            {
                tileHeight = value;
                Raise(nameof(TileHeight));
            }
        }
    }

    private bool isSpeaking;
    public bool IsSpeaking
    {
        get => isSpeaking;
        set
        {
            if (isSpeaking == value)
            {
                return;
            }
            isSpeaking = value;
            Raise(nameof(IsSpeaking));
            Raise(nameof(SpeakingRingVisibility));
        }
    }

    public Visibility SpeakingRingVisibility => isSpeaking ? Visibility.Visible : Visibility.Collapsed;

    private bool isMuted;
    public bool IsMuted
    {
        get => isMuted;
        set
        {
            if (isMuted == value)
            {
                return;
            }
            isMuted = value;
            Raise(nameof(IsMuted));
            Raise(nameof(MuteBadgeVisibility));
        }
    }

    public Visibility MuteBadgeVisibility => isMuted ? Visibility.Visible : Visibility.Collapsed;

    private bool isCameraOff;
    public bool IsCameraOff
    {
        get => isCameraOff;
        set
        {
            if (isCameraOff == value)
            {
                return;
            }
            isCameraOff = value;
            Raise(nameof(IsCameraOff));
            Raise(nameof(CameraOffBadgeVisibility));
            Raise(nameof(VideoVisibility));
        }
    }

    public Visibility CameraOffBadgeVisibility => isCameraOff ? Visibility.Visible : Visibility.Collapsed;

    // Sharing the screen puts the screen on the video track INSTEAD of the camera — so a sharer
    // usually has the camera off, and the video must still show.
    private bool isScreenSharing;
    public bool IsScreenSharing
    {
        get => isScreenSharing;
        set
        {
            if (isScreenSharing == value)
            {
                return;
            }
            isScreenSharing = value;
            Raise(nameof(IsScreenSharing));
            Raise(nameof(VideoVisibility));
            Raise(nameof(VideoStretch));
        }
    }

    // Hide the live-video surface when nothing is being sent (camera off and no screen share), so the
    // initials avatar shows instead of the last decoded frame frozen in place.
    public Visibility VideoVisibility => isCameraOff && !isScreenSharing ? Visibility.Collapsed : Visibility.Visible;

    // A shared screen is shown whole (letterboxed), never cropped: every corner of it must be
    // reachable when the viewer controls it. A camera keeps filling the tile.
    public Microsoft.UI.Xaml.Media.Stretch VideoStretch => isScreenSharing
        ? Microsoft.UI.Xaml.Media.Stretch.Uniform
        : Microsoft.UI.Xaml.Media.Stretch.UniformToFill;

    // Remote screen control: this member shares and lets us control; we are controlling them now.
    private bool letsMeControl;
    public bool LetsMeControl
    {
        get => letsMeControl;
        set
        {
            if (letsMeControl == value)
            {
                return;
            }
            letsMeControl = value;
            Raise(nameof(LetsMeControl));
            Raise(nameof(ControlButtonVisibility));
        }
    }

    private bool isControlling;
    public bool IsControlling
    {
        get => isControlling;
        set
        {
            if (isControlling == value)
            {
                return;
            }
            isControlling = value;
            Raise(nameof(IsControlling));
            Raise(nameof(ControlButtonText));
        }
    }

    public Visibility ControlButtonVisibility => letsMeControl ? Visibility.Visible : Visibility.Collapsed;
    public string ControlButtonText => isControlling ? "Перестать управлять" : "Управлять";

    public event System.ComponentModel.PropertyChangedEventHandler? PropertyChanged;

    private void Raise(string name)
        => PropertyChanged?.Invoke(this, new System.ComponentModel.PropertyChangedEventArgs(name));
}
