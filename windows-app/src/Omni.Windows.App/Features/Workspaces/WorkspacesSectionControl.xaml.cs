using System;
using System.Collections.Generic;
using System.Collections.ObjectModel;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Text;
using System.Threading.Tasks;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Omni.Windows.App.Core;
using Omni.Windows.App.Presentation;
using Windows.ApplicationModel.DataTransfer;
using Windows.Storage;

namespace Omni.Windows.App.Features.Workspaces;

/// One row of the workspace list, drawn by the chat and contact lists' row (App.xaml,
/// PersonRowTemplate): the names it binds, the marks it has no use for collapsed.
public sealed class WorkspaceListItem
{
    internal WorkspaceListItem(GroupSummary group)
    {
        GroupId = group.GroupId;
        Name = group.Name;
        AvatarText = MainWindowPresentation.BuildAvatarText(group.Name);
        PreviewText = $"{group.ActiveMemberCount} {Groups.GroupConversationControl.MembersWord(group.ActiveMemberCount)}";
        Signature = $"{group.GroupId}|{group.Name}|{group.ActiveMemberCount}";
    }

    public string GroupId { get; }
    public string Name { get; }
    internal string Signature { get; }
    public string Title => Name;
    public string AvatarText { get; }
    public string PreviewText { get; }
    public Brush AvatarBrush => AvatarPalette.BrushFor(GroupId);
    public string TimeText => string.Empty;
    public string CornerText => string.Empty;
    public Visibility AvatarLoadingVisibility => Visibility.Collapsed;
    public Visibility GroupMarkVisibility => Visibility.Collapsed;
    public Visibility PresenceDotVisibility => Visibility.Collapsed;
    public Visibility UnreadBadgeVisibility => Visibility.Collapsed;
}

/// The Workspaces section (docs/decisions/omni-workspace-ux.md, phase 1): every repository of the
/// workspace with its shared main, what waits for an approval, members' branches; «Забрать», «Забрать
/// сюда», «Опубликовать ветку», «Одобрить»; the admin's rule. Everything is read from the core; git
/// stays git — the person pushes and merges in the IDE, Omni shows what happened and does the frequent
/// steps in one click. A folder of files (models, drawings, documents — phase 2) shows versions instead:
/// «Изменено у тебя — Сохранить версию», what the next version waits for; no git words.
public sealed partial class WorkspacesSectionControl : UserControl
{
    private RustBridge? bridge;
    private readonly ObservableCollection<WorkspaceListItem> items = new();
    private string? groupId;
    private GroupSummary? group;
    private WorkspaceScreen? screen;
    private string signature = string.Empty;
    private Task? loading;
    private bool reloadWanted;
    private bool selectingFromCode;
    // A read failed and said so: the next good read takes the red bar away (it told about a start).
    private bool readFailed;
    private string? workspaceRootGroup;

    // The page re-reads every 2 s while the section is on screen: the core runs a few git commands.
    private readonly DispatcherTimer refreshTimer = new() { Interval = TimeSpan.FromSeconds(2) };
    private bool readingGroups;

    /// The window's folder dialog (the control has no window of its own); null = cancelled.
    public Func<Task<string?>>? PickFolder { get; set; }
    public Func<string, string?>? PickSaveFile { get; set; }

    /// The window's people picker (group id, group name → chosen 1:1 dialog keys, null = cancelled).
    public Func<string, string, Task<IReadOnlyList<string>?>>? PickPeople { get; set; }

    /// «Чат»: the shell opens the workspace's group in Chats.
    public event Action<string>? OpenChatRequested;

    /// A workspace was made or changed in a way the chat list shows (its mark).
    public event Action? WorkspacesChanged;

    public WorkspacesSectionControl()
    {
        InitializeComponent();
        WorkspacesList.ItemsSource = items;
        // One list-column width for every section (Shared.SectionRail).
        Loaded += (_, _) =>
        {
            Shared.SectionRail.Changed -= ApplyRailWidth;
            Shared.SectionRail.Changed += ApplyRailWidth;
            ApplyRailWidth();
        };
        Unloaded += (_, _) => Shared.SectionRail.Changed -= ApplyRailWidth;
        // A tick while a read is under way waits for the next one: queued, reads ran back to back.
        refreshTimer.Tick += (_, _) =>
        {
            if (loading is not { IsCompleted: false })
            {
                _ = RefreshOffThreadAsync();
            }
        };
        WorkspaceHeader.ShowBack = false;
        // «Проекты» pressed again on a project's page: the list — the tab is the way back.
        bool projectsWasChosen = false;
        WorkspaceProjectsTab.AddHandler(PointerPressedEvent, new Microsoft.UI.Xaml.Input.PointerEventHandler((_, _) => projectsWasChosen = WorkspaceProjectsTab.IsSelected), true);
        WorkspaceProjectsTab.Tapped += (_, _) =>
        {
            if (projectsWasChosen && (codeRepo is not null || (ProjectOpen && !OnPlaceOfItsOwn))) BackToCards();
        };
        Unloaded += (_, _) => refreshTimer.Stop();
    }

    internal void Attach(RustBridge rustBridge) => bridge = rustBridge;

    /// The section came on screen: read the list, start the refresh.
    internal void Activate()
    {
        Visibility = Visibility.Visible;
        _ = RefreshOffThreadAsync();
        refreshTimer.Start();
    }

    /// Read and draw while still hidden: the first read of a workspace runs git for each project (over a
    /// second), and the first visit to the section stood empty for that long.
    internal Task PreloadAsync() => RefreshOffThreadAsync();

    internal void Deactivate()
    {
        Visibility = Visibility.Collapsed;
        refreshTimer.Stop();
    }

    /// Open one workspace (from a group's page, the test channel) — a group without code yet too:
    /// its page offers to bind a folder.
    internal void Select(string id)
    {
        if (groupId != id)
        {
            if (ProjectOpen) BackToCards();
            groupId = id;
            screen = null;
            signature = string.Empty;
            WorkspaceCodePanel.Children.Clear();
            SetStatus(null);
            _ = ShowOverviewAsync();
        }
        Refresh();
    }

    private void Refresh()
    {
        // The core still starting is a wait, not a failure: the next tick reads again.
        if (bridge is null || !bridge.IsInitialized)
        {
            return;
        }

        try
        {
            ShowGroups(bridge.ListGroups());
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось прочитать workspaces: {ex.Message}", StatusKind.Failed);
            readFailed = true;
        }
    }

    // The same, for the section coming on screen and the tick: a read waiting for the database under a
    // background write must not hold the window. Refresh() stays for code that reads the selection right after.
    private async Task RefreshOffThreadAsync()
    {
        if (bridge is not { IsInitialized: true } core || readingGroups)
        {
            return;
        }

        readingGroups = true;
        try
        {
            ShowGroups(await Task.Run(() => core.ListGroups()));
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось прочитать workspaces: {ex.Message}", StatusKind.Failed);
            readFailed = true;
        }
        finally
        {
            readingGroups = false;
        }
    }

    private void ShowGroups(IEnumerable<GroupSummary> all)
    {
        List<GroupSummary> groups = all.Where(g => g.MyStatus == "member").ToList();
        string query = WorkspaceSearchBox.Text.Trim();
        List<WorkspaceListItem> fresh = groups
            .Where(g => g.IsWorkspace || g.GroupId == groupId)
            .Where(g => query.Length == 0 || g.Name.Contains(query, StringComparison.CurrentCultureIgnoreCase) || g.GroupId == groupId)
            .OrderBy(g => g.Name, StringComparer.CurrentCultureIgnoreCase)
            .Select(g => new WorkspaceListItem(g))
            .ToList();
        if (!fresh.Select(i => i.Signature).SequenceEqual(items.Select(i => i.Signature)))
        {
            selectingFromCode = true;
            items.Clear();
            foreach (WorkspaceListItem item in fresh)
            {
                items.Add(item);
            }
            selectingFromCode = false;
        }
        WorkspacesEmptyHint.Visibility = items.Count == 0 ? Visibility.Visible : Visibility.Collapsed;
        groupId ??= items.FirstOrDefault()?.GroupId;
        group = groups.FirstOrDefault(g => g.GroupId == groupId);
        if (group is null)
        {
            groupId = null;
        }

        selectingFromCode = true;
        WorkspacesList.SelectedItem = items.FirstOrDefault(i => i.GroupId == groupId);
        selectingFromCode = false;
        WorkspacePage.Visibility = group is null ? Visibility.Collapsed : Visibility.Visible;
        WorkspaceNothingSelected.Visibility = group is null ? Visibility.Visible : Visibility.Collapsed;
        if (group is not null)
        {
            WorkspaceHeader.Title.Text = group.Name;
            WorkspaceHeader.Subtitle.Text = $"{group.ActiveMemberCount} {Groups.GroupConversationControl.MembersWord(group.ActiveMemberCount)}";
            WorkspaceHeader.Avatar.Initials.Text = MainWindowPresentation.BuildAvatarText(group.Name);
            WorkspaceHeader.Avatar.Circle.Background = AvatarPalette.BrushFor(group.GroupId);
            _ = LoadAsync();
        }
    }

    private void OnSearchChanged(object sender, TextChangedEventArgs e) => Refresh();

    private void OnWorkspaceSelected(object sender, SelectionChangedEventArgs e)
    {
        if (!selectingFromCode && WorkspacesList.SelectedItem is WorkspaceListItem item)
        {
            Select(item.GroupId);
        }
    }

    /// Read the workspace off the UI thread (the core runs git), then draw it. A read asked for while
    /// one runs happens right after it, so a button's result is never drawn from the older read.
    private Task LoadAsync()
    {
        if (loading is { IsCompleted: false })
        {
            reloadWanted = true;
            return loading;
        }
        loading = LoadLoopAsync();
        return loading;
    }

    private async Task LoadLoopAsync()
    {
        do
        {
            reloadWanted = false;
            if (bridge is null || groupId is null)
            {
                return;
            }
            string id = groupId;
            RustBridge core = bridge;
            try
            {
                if (workspaceRootGroup != id)
                {
                    await WorkspaceFolderPreferences.EnsureAsync(core);
                    workspaceRootGroup = id;
                }
                long started = System.Diagnostics.Stopwatch.GetTimestamp();
                WorkspaceScreen fresh = await Task.Run(() => core.GetWorkspaceScreen(id));
                AppTrace.Write("workspace/view", $"read {System.Diagnostics.Stopwatch.GetElapsedTime(started).TotalMilliseconds:F0}ms");
                if (groupId == id)
                {
                    screen = fresh;
                    if (readFailed) { readFailed = false; SetStatus(null); }
                    long drawStarted = System.Diagnostics.Stopwatch.GetTimestamp();
                    Draw(fresh);
                    double drew = System.Diagnostics.Stopwatch.GetElapsedTime(drawStarted).TotalMilliseconds;
                    if (drew >= 30) AppTrace.Write("workspace/view", $"draw {drew:F0}ms");
                }
            }
            catch (Exception ex)
            {
                AppTrace.Write("workspace/view", $"draw failed: {ex}");
                SetStatus($"Не удалось загрузить проекты workspace: {ex.Message}", StatusKind.Failed);
                readFailed = true;
            }
        }
        while (reloadWanted);
    }

    private void Draw(WorkspaceScreen view)
    {
        // What our agents wait for from us, on the switcher whatever place is open: «Обзор» is where it
        // is answered. «main обновился» is its project's: on its row and on its page.
        WorkspaceOverviewTab.Text = view.AgentAsks > 0 ? $"Обзор · {view.AgentAsks}" : "Обзор";

        string fresh = Signature(view);
        if (fresh == signature)
        {
            return;
        }
        signature = fresh;
        WorkspaceCodePanel.Children.Clear();
        projectScreenChanged?.Invoke(view);
        if (codeRepo is not null && view.Repos.FirstOrDefault(r => r.Repo == codeRepo && r.Here && !r.Files) is { } code)
        {
            DrawCodePage(view, code);
            return;
        }
        codeRepo = null;

        // The projects, to choose what to work on; what waits and what happened are on «Обзор». The tasks
        // project is the board behind «Задачи», not a row.
        if (!ProjectOpen) SetPlaceBar(PlainButton("Добавить папку", accent: false, () => _ = BindFolderAsync()));
        List<WorkspaceRepoState> projects = view.Repos.Where(r => !r.IsTasks).ToList();
        StackPanel main = new() { Spacing = 12 };
        if (projects.Count == 0)
        {
            main.Children.Add(Body("В группе ещё нет проекта. Привяжите папку: с кодом (git) — участники увидят её main и ветки; с файлами — моделями, чертежами, документами — у всех будет её последняя версия."));
            main.Children.Add(PlainButton("Привязать папку…", accent: true, () => _ = BindFolderAsync()));
        }
        else
        {
            Features.Shared.PageColumns rows = new() { Spacing = 6, ColumnGap = 12, MinColumn = 480, EachChild = true };
            foreach (WorkspaceRepoState repo in projects)
            {
                FrameworkElement row = ProjectRow(repo);
                row.Tag = repo.Repo;
                rows.Children.Add(row);
            }
            main.Children.Add(rows);
        }
        WorkspaceCodePanel.Children.Add(main);
    }

    // A code project's buttons: its folder, «Опубликовать ветку», «На мои устройства», «Забрать N коммитов».
    private StackPanel CodeActions(WorkspaceScreen view, WorkspaceRepoState repo)
    {
        StackPanel actions = new() { Orientation = Orientation.Horizontal, Spacing = 8 };
        if (repo.WorkPath is { } folder)
        {
            actions.Children.Add(IconButton("", "Открыть папку", () => OpenFolder(folder)));
            List<WorkspaceLocalBranch> unpublished = repo.LocalBranches.Where(b => !b.Published).ToList();
            Button publish = IconButton("", "Опубликовать ветку", null);
            // Nothing to publish: no button (a grey one only says «not now»).
            publish.Visibility = unpublished.Count > 0 ? Visibility.Visible : Visibility.Collapsed;
            ToolTipService.SetToolTip(publish, "Отправить ветку из вашей папки участникам");
            MenuFlyout menu = new() { Placement = Microsoft.UI.Xaml.Controls.Primitives.FlyoutPlacementMode.Bottom };
            foreach (WorkspaceLocalBranch branch in unpublished)
            {
                MenuFlyoutItem item = new() { Text = $"{branch.Branch} · {Shorten(branch.Subject, 40)}" };
                item.Click += (_, _) => _ = RunAsync(
                    core => core.PublishWorkspaceBranch(groupId!, repo.Repo, branch.Branch),
                    $"Ветка {branch.Branch} опубликована.",
                    "опубликовать ветку");
                menu.Items.Add(item);
            }
            publish.Flyout = menu;
            actions.Children.Add(publish);

            // «На мои устройства»: a branch for the account's other devices only, before the team sees it.
            if (view.IHaveOtherDevices)
            {
                List<WorkspaceLocalBranch> toSend = repo.LocalBranches.Where(b => !b.Published && !b.OnMyDevices).ToList();
                Button send = IconButton("", "На мои устройства", null);
                send.Visibility = toSend.Count > 0 ? Visibility.Visible : Visibility.Collapsed;
                ToolTipService.SetToolTip(send, "Отправить ветку только на свои устройства — команда её не увидит");
                MenuFlyout sendMenu = new() { Placement = Microsoft.UI.Xaml.Controls.Primitives.FlyoutPlacementMode.Bottom };
                foreach (WorkspaceLocalBranch branch in toSend)
                {
                    MenuFlyoutItem item = new() { Text = $"{branch.Branch} · {Shorten(branch.Subject, 40)}" };
                    item.Click += (_, _) => _ = RunAsync(
                        core => core.SendWorkspaceBranchToMyDevices(groupId!, repo.Repo, branch.Branch),
                        $"Ветка {branch.Branch} — на ваших устройствах. Команда её не видит, пока не опубликуете.",
                        "отправить на свои устройства");
                    sendMenu.Items.Add(item);
                }
                send.Flyout = sendMenu;
                actions.Children.Add(send);
            }

            // «main обновился» is the page's one urgent thing: its primary button, last on the strip; the
            // missing commits are at the top of «История main» beside.
            if (repo.MainBehind > 0)
            {
                Button pull = PlainButton($"Забрать {repo.MainBehind} {CommitsWord(repo.MainBehind)}", true, () => _ = PullAsync(repo.Repo));
                ToolTipService.SetToolTip(pull, repo.Main is { } newest && !string.IsNullOrEmpty(newest.Author)
                    ? $"main обновился: в вашей папке их не хватает, последний — от {newest.Author}."
                    : "main обновился: в вашей папке их не хватает.");
                actions.Children.Add(pull);
            }
        }
        return actions;
    }

    /// A code project's page (in the projects' place), in the order of work: what asks for something
    /// (its folder behind, a main to approve, a fork, a device's branch), then the shared main and the
    /// members' branches, then the project's rule.
    private FrameworkElement CodeBody(WorkspaceScreen view, WorkspaceRepoState repo)
    {
        StackPanel card = new() { Spacing = 8 };
        if (repo.WorkPath is null) card.Children.Add(Caption("Своей папки у репозитория здесь нет — код лежит только в Omni."));

        foreach (WorkspaceTip wait in repo.Waiting)
        {
            string what = wait.Commit is { } c ? $"«{c.Subject}»" : Short(wait.Oid);
            card.Children.Add(wait.IsMe
                ? Caption($"Ваш main {what} ждёт одобрения другого участника.")
                : ActionRow(
                    $"{wait.Name} двигает main на {what} — ждёт одобрения.",
                    "Одобрить",
                    () => _ = RunAsync(
                        core => core.ApproveWorkspaceCommit(groupId!, repo.Repo, wait.Oid),
                        "Одобрено — main сдвинется у всех.",
                        "одобрить")));
        }
        if (repo.Forks.Count > 0)
        {
            string who = string.Join(" и ", repo.Forks.Select(f => f.IsMe ? "вы" : f.Name).Distinct());
            card.Children.Add(new InfoBar
            {
                IsOpen = true,
                IsClosable = false,
                Severity = InfoBarSeverity.Warning,
                Title = "main спорный",
                Message = view.IAmMaintainer
                    ? $"{who} двинули его по-разному. Сведите ветки в IDE и опубликуйте main."
                    : $"{who} двинули его по-разному — решают сопровождающие.",
            });
        }

        // From our other devices: what they sent «на мои устройства» and this folder does not have yet.
        foreach (WorkspaceDeviceBranch theirs in (repo.DeviceBranches ?? new()).Where(b => b.State is not ("same" or "older")))
        {
            string what = theirs.Commit is { } c ? $" · «{Shorten(c.Subject, 50)}»" : string.Empty;
            if (theirs.State == "diverged")
            {
                card.Children.Add(new InfoBar
                {
                    IsOpen = true,
                    IsClosable = false,
                    Severity = InfoBarSeverity.Warning,
                    Title = $"{theirs.Branch} разошлась",
                    Message = $"На устройстве «{theirs.Device}» и здесь разные коммиты. Сведите в IDE: omni-devices/{theirs.Node}/heads/{theirs.Branch}.",
                });
                continue;
            }
            string sentence = theirs.State switch
            {
                "new" => $"С «{theirs.Device}»: {theirs.Branch}{what} — здесь такой ветки нет.",
                "newer" => $"С «{theirs.Device}»: {theirs.Branch}{what} — новее, чем здесь.",
                _ => $"С «{theirs.Device}»: {theirs.Branch}{what}.",
            };
            card.Children.Add(ActionRow(sentence, "Забрать", () => _ = TakeFromDeviceAsync(repo.Repo, theirs)));
        }

        card.Children.Add(SectionTitle("Общий main", first: card.Children.Count == 0));
        card.Children.Add(repo.Main is { } main
            ? Bounded(Line($"«{main.Subject}» · {main.Author} · {MainWindowPresentation.FormatMessageTime(main.AtUnixMs)}", Short(main.Oid)), CommitLineMax)
            : Caption(repo.Waiting.Count > 0 ? "Ещё нет: первый main ждёт одобрения." : "Ещё нет: ни один сопровождающий не опубликовал main."));

        // Members' branches, by person.
        if (repo.Branches.Count > 0)
        {
            card.Children.Add(SectionTitle("Ветки участников"));
            foreach (WorkspaceTip tip in repo.Branches.OrderBy(b => b.Name, StringComparer.CurrentCultureIgnoreCase).ThenBy(b => b.Branch, StringComparer.Ordinal))
            {
                string state = tip.Commit is { } c
                    ? $"«{Shorten(c.Subject, 60)}» · {MainWindowPresentation.FormatMessageTime(c.AtUnixMs)}"
                    : "забираю…";
                card.Children.Add(Bounded(Line($"{tip.Name}: {tip.Branch} · {state}", Short(tip.Oid)), CommitLineMax));
            }
        }

        // The rule is the project's own: an admin switches it here, the others read it.
        card.Children.Add(SectionTitle("Правило main"));
        if (view.IAmAdmin)
        {
            ToggleSwitch rule = new() { IsOn = repo.NeedsApproval, OnContent = string.Empty, OffContent = string.Empty, MinWidth = 0 };
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(rule, $"Правило main проекта {repo.Repo}");
            rule.Toggled += (_, _) =>
            {
                if (rule.IsOn != repo.NeedsApproval)
                {
                    bool needsApproval = rule.IsOn;
                    _ = RunAsync(
                        core => core.SetWorkspaceMainRule(groupId!, repo.Repo, needsApproval),
                        needsApproval
                            ? $"{repo.Repo}: теперь main двигается только после одобрения ещё одного участника."
                            : $"{repo.Repo}: теперь main двигает любой сопровождающий.",
                        "сменить правило");
                }
            };
            card.Children.Add(new Shared.SettingRow
            {
                Header = "Одобрение ещё одного участника",
                Description = repo.NeedsApproval
                    ? "Включено: main двигается только после одобрения."
                    : "Выключено: main двигает любой сопровождающий, только вперёд.",
                Control = rule,
            });
        }
        else
        {
            card.Children.Add(Caption(repo.NeedsApproval
                ? "main двигает сопровождающий, и только после одобрения ещё одного участника."
                : "main двигает любой сопровождающий (админ группы), только вперёд."));
        }
        return card;
    }

    /// Beside a code project's state: what the shared main took lately — what changed in the common
    /// version while the person looks at their folder and the branches.
    private FrameworkElement CodeHistory(WorkspaceRepoState repo)
    {
        StackPanel area = new() { Spacing = 10 };
        area.Children.Add(SectionTitle("История main", first: true));
        List<WorkspaceCommit> commits = repo.Recent ?? [];
        if (commits.Count == 0)
        {
            area.Children.Add(Caption("Коммиты общего main появятся здесь."));
            return area;
        }
        foreach (WorkspaceCommit commit in commits)
        {
            StackPanel one = new() { Spacing = 1 };
            one.Children.Add(new TextBlock { Text = commit.Subject, TextWrapping = TextWrapping.Wrap, MaxLines = 2, TextTrimming = TextTrimming.CharacterEllipsis });
            one.Children.Add(Caption($"{commit.Author} · {MainWindowPresentation.FormatMessageTime(commit.AtUnixMs)} · {Short(commit.Oid)}"));
            area.Children.Add(one);
        }
        return area;
    }

    /// What a files project's page shows above its tabs, in the order of work: what is in the way, then
    /// what is mine — «Изменения в твоей папке» with «Сохранить версию». Nothing when all is calm: the
    /// folder follows the shared version by itself.
    /// What is typed under «Есть изменения» and whether the taken files go back: it outlives the
    /// redraws of the strip (every read of the workspace that changes something draws it anew).
    private sealed class SaveDraft
    {
        public string Text = string.Empty;
        public bool Give;
    }

    private StackPanel FilesWork(WorkspaceRepoState repo, Action showChanged, SaveDraft draft)
    {
        StackPanel card = new() { Spacing = 10 };

        if (screen?.CustodyAsked?.Contains(repo.Repo) == true)
            card.Children.Add(new InfoBar { IsOpen = true, IsClosable = false, Severity = InfoBarSeverity.Informational,
                Title = "Вам предлагают хранить проект", Message = "Ответ — во вкладке «Настройки»: там размер истории, выбор папки и согласие." });

        // Where the person's work is (docs/decisions/workspace-files-storage.md): changed and not saved,
        // saved and not accepted, accepted, here on this computer — one line each, a chip to scan for.
        WorkspaceFilesSync sync = repo.Sync ?? new WorkspaceFilesSync(new(), new(), 0, 0, null);
        if (!string.IsNullOrEmpty(sync.Problem))
            card.Children.Add(StateLine("Не обновляется", "Critical", $"Папка не обновляется: {sync.Problem}"));
        if (repo.Own is { } own)
        {
            string which = $"Ваша версия «{Shorten(own.Commit.Subject, 60)}» · {MainWindowPresentation.FormatMessageTime(own.Commit.AtUnixMs)}"
                + (own.Count > 1 ? $" (и ещё {own.Count - 1} до неё)" : string.Empty);
            card.Children.Add(own.State switch
            {
                "rejected" => StateLine("Не принята", "Critical",
                    $"{which} не принята. В папке остаётся она, у участников — общая версия {repo.Version}. Поправьте файлы и сохраните снова — новую версию посмотрят ещё раз — или верни папку к общей."),
                "joining" => StateLine("Принимается", "Accent", $"{which} становится общей."),
                _ => StateLine("Ждёт принятия", "Accent",
                    $"{which} — в папке она. Участники получат её, когда админ или хранитель примет."),
            });
            // Not accepted yet or turned down: one button takes it back, instead of editing and saving again.
            if (own.State is "rejected" or "waiting")
            {
                Button back = PlainButton("Вернуть к общей…", accent: false, () => _ = BackToSharedAsync(repo));
                back.HorizontalAlignment = HorizontalAlignment.Left;
                card.Children.Add(back);
            }
        }
        if (sync.ContentMissingBytes > 0)
        {
            card.Children.Add(StateLine("Не всё здесь", "Caution",
                $"Не хватает {Megabytes(sync.ContentMissingBytes)} из {Megabytes(sync.ContentTotalBytes)}. Заберу, когда участники с ними будут в сети."));
        }
        if (sync.WaitingOpen.Count > 0)
        {
            card.Children.Add(new InfoBar
            {
                IsOpen = true,
                IsClosable = false,
                Severity = InfoBarSeverity.Informational,
                Title = "Новая версия ждёт",
                Message = $"Закройте {Names(sync.WaitingOpen)} — папка обновится целиком.",
            });
        }
        if (sync.WaitingChanged.Count > 0)
        {
            card.Children.Add(new InfoBar
            {
                IsOpen = true,
                IsClosable = false,
                Severity = InfoBarSeverity.Warning,
                Title = "Новая версия меняет то же, что и вы",
                Message = $"У вас изменены {Names(sync.WaitingChanged)}. Сохраните свою версию: если вы меняли одно и то же, останутся обе — ваша рядом.",
            });
        }
        // What someone else took first (mine are marked on the files' rows).
        if (repo.HeldElsewhere is { Count: > 0 } held)
            card.Children.Add(new InfoBar
            {
                IsOpen = true,
                IsClosable = false,
                Severity = InfoBarSeverity.Warning,
                Title = "Файл взяли раньше",
                Message = string.Join("; ", held.Select(h => $"{h.Path} — в работе у {h.Name}"))
                    + ". Ваша правка разойдётся с его: отложите её рядом — у файла.",
            });

        if (repo.WorkPath is not null)
        {
            if (repo.ChangedCount > 0)
            {
                List<string> changed = repo.Changed ?? new();
                card.Children.Add(StateWithAction($"Есть изменения · {repo.ChangedCount}", "Caution",
                    string.Join(", ", changed.Take(3).Select(Path.GetFileName)) + (repo.ChangedCount > 3 ? $" и ещё {repo.ChangedCount - 3}" : string.Empty),
                    "Показать", showChanged));
                Grid save = new() { ColumnSpacing = 12 };
                save.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
                save.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
                TextBox comment = new() { PlaceholderText = "Что изменилось", Text = draft.Text };
                Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(comment, $"Что изменилось в {repo.Repo}");
                comment.TextChanged += (_, _) => draft.Text = comment.Text;
                save.Children.Add(comment);
                // Saving keeps the files taken (set 2026-11-01): give them back here at once.
                // Not a command of its own: what the save does after it, and to which files.
                List<string> mineTaken = repo.Taken ?? new();
                CheckBox give = new() { Content = $"После сохранения отдать {Names(mineTaken)}", IsChecked = draft.Give, Visibility = mineTaken.Count > 0 ? Visibility.Visible : Visibility.Collapsed };
                give.Click += (_, _) => draft.Give = give.IsChecked == true;
                Button? go = null;
                async Task SaveAndClear()
                {
                    await SaveVersionAsync(repo.Repo, draft.Text, go, draft.Give ? repo.Taken : null);
                    draft.Text = string.Empty;
                    draft.Give = false;
                }
                go = PlainButton("Сохранить версию", accent: true, () => _ = SaveAndClear());
                Grid.SetColumn(go, 1);
                save.Children.Add(go);
                card.Children.Add(save);
                card.Children.Add(give);
            }
        }

        return card;
    }

    // A state line with the one action it asks for, on the right.
    private FrameworkElement StateWithAction(string chip, string tone, string text, string action, Action go)
    {
        Grid line = new() { ColumnSpacing = 12 };
        line.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        line.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        FrameworkElement state = StateLine(chip, tone, text);
        state.VerticalAlignment = VerticalAlignment.Center;
        line.Children.Add(state);
        Button button = PlainButton(action, accent: false, go);
        Grid.SetColumn(button, 1);
        line.Children.Add(button);
        return line;
    }

    // «Требует внимания» (docs/decisions/omni-workspace-ux.md): what in the files projects waits for
    // this person — a decision, an answer, a closed file, a broken folder — at the top, each with
    // its way there. Not the unsaved changes: that is ordinary work, the card shows it. None: no block.
    private FrameworkElement? AttentionCard(WorkspaceScreen view)
    {
        StackPanel items = new() { Spacing = 10 };
        // By urgency across all projects (review 2026-11-01): what stops work first — a folder not
        // updating, a version turned down, an edit about to cross — then what waits for a choice, then
        // a decision on someone's version, then a question. Within one kind — in the projects' order.
        List<(int Rank, int Order, Action Draw)> found = [];
        int Urgency(string chip) => chip switch
        {
            "Не обновляется" => 0,
            "Не принята" => 1,
            "Пересечение" => 2,
            "Ждёт файла" => 3,
            "Расхождение" => 4,
            "Ждёт решения" => 5,
            _ => 6,
        };
        void Add(string chip, string tone, string text, string action, Action go) =>
            found.Add((Urgency(chip), found.Count, () => Line(chip, tone, text, action, go)));
        void Line(string chip, string tone, string text, string action, Action go)
        {
            Grid line = new() { ColumnSpacing = 10 };
            line.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            line.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            line.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            Border mark = Chip(chip, tone);
            mark.VerticalAlignment = VerticalAlignment.Center;
            line.Children.Add(mark);
            TextBlock what = new() { Text = text, TextWrapping = TextWrapping.Wrap, VerticalAlignment = VerticalAlignment.Center };
            Grid.SetColumn(what, 1);
            line.Children.Add(what);
            Button button = PlainButton(action, accent: false, go);
            Grid.SetColumn(button, 2);
            line.Children.Add(button);
            items.Children.Add(line);
        }
        foreach (WorkspaceRepoState repo in view.Repos.Where(r => r.Files && !r.IsTasks))
        {
            string name = repo.Repo;
            void ToCard() => _ = ShowFilesAsync(name);
            WorkspaceFilesSync? sync = repo.Here ? repo.Sync : null;
            if (!string.IsNullOrEmpty(sync?.Problem))
                Add("Не обновляется", "Critical", $"Папка «{name}»: {sync.Problem}", "К проекту", ToCard);
            if (sync?.WaitingChanged.Count > 0)
                Add("Пересечение", "Caution", $"Новая версия «{name}» меняет то же, что и вы: {Names(sync.WaitingChanged)}. Сохраните свою — останутся обе.", "К проекту", ToCard);
            if (repo.Own is { State: "rejected" } own)
                Add("Не принята", "Critical", $"Вашу версию «{Shorten(own.Commit.Subject, 50)}» в «{name}» не приняли.", "К проекту", ToCard);
            bool reviewer = view.IAmAdmin || view.CustodianOf?.Contains(name) == true;
            if (reviewer && repo.Waiting.Count > 0)
            {
                WorkspaceTip first = repo.Waiting[0];
                string version = first.Commit is { } c ? $"версию «{Shorten(c.Subject, 50)}»" : "версию";
                string more = repo.Waiting.Count > 1 ? $" Ещё ждут: {repo.Waiting.Count - 1}." : string.Empty;
                Add("Ждёт решения", "Accent", $"{first.Name} предлагает {version} в «{name}».{more}", "Проверить предложение", () => _ = ShowFilesAsync(name, proposal: first.Oid));
            }
            if (view.CustodyAsked?.Contains(name) == true)
                Add("Вопрос", "Accent", $"Вам предлагают хранить «{name}»: историю проекта на этом компьютере.", "Ответить…", () => _ = ShowCustodyAsync(name));
            if (sync?.WaitingOpen.Count > 0)
                Add("Ждёт файла", "Caution", $"Новая версия «{name}» ждёт: закройте {Names(sync.WaitingOpen)}.", "К проекту", ToCard);
            if (reviewer && repo.RevisionsOnCheck is { Count: > 0 } onCheck)
                Add("Нормоконтроль", "Caution", $"В «{name}» на нормоконтроле: ревизия {string.Join(", ", onCheck)}.", "Проверить ревизию", () => _ = ShowHistoryAsync(name, pending: true));
            if (repo.Conflicts is { Count: > 0 } conflicts)
                Add("Расхождение", "Caution", $"В «{name}» разошлись правки {Names(conflicts)}: выберите итоговую.", "Выбрать…", () => _ = ShowFilesAsync(name, conflict: conflicts[0]));
        }
        foreach ((int _, int _, Action draw) in found.OrderBy(f => f.Rank).ThenBy(f => f.Order))
            draw();
        if (items.Children.Count == 0)
        {
            return null;
        }
        StackPanel card = new() { Spacing = 12 };
        card.Children.Add(new TextBlock { Text = "Требует внимания", Style = CardTitle });
        card.Children.Add(items);
        return Card(card);
    }

    // Scroll the page to a project's card (the test channel's ws.show).
    internal bool BringRepoIntoView(string repo)
    {
        static IEnumerable<FrameworkElement> Under(DependencyObject at)
        {
            for (int i = 0; i < Microsoft.UI.Xaml.Media.VisualTreeHelper.GetChildrenCount(at); i++)
            {
                DependencyObject child = Microsoft.UI.Xaml.Media.VisualTreeHelper.GetChild(at, i);
                if (child is FrameworkElement element) yield return element;
                foreach (FrameworkElement deeper in Under(child)) yield return deeper;
            }
        }
        FrameworkElement? card = Under(WorkspaceCodePanel).FirstOrDefault(c => c.Tag as string == repo);
        card?.StartBringIntoView(new BringIntoViewOptions { VerticalAlignmentRatio = 0, AnimationDesired = false });
        return card is not null;
    }

    internal static string FilesWord(int n)
    {
        int mod100 = n % 100;
        int mod10 = n % 10;
        if (mod100 is >= 11 and <= 14)
        {
            return "файлов";
        }
        return mod10 switch
        {
            1 => "файл",
            >= 2 and <= 4 => "файла",
            _ => "файлов",
        };
    }

    private static string Names(List<string> paths) =>
        string.Join(", ", paths.Take(3).Select(Path.GetFileName)) + (paths.Count > 3 ? $" и ещё {paths.Count - 3}" : string.Empty);

    private static string Megabytes(long bytes) => bytes switch
    {
        <= 0 => "0 КБ",
        >= 1 << 20 => $"{bytes / (double)(1 << 20):0.#} МБ",
        _ => $"{Math.Max(1, bytes / 1024)} КБ",
    };


    private void OnOpenChatClicked(object sender, RoutedEventArgs e)
    {
        if (groupId is not null)
        {
            OpenChatRequested?.Invoke(groupId);
        }
    }

    private async void OnInviteClicked(object sender, RoutedEventArgs e)
    {
        if (bridge is null || group is null || PickPeople is null)
        {
            return;
        }

        try
        {
            IReadOnlyList<string>? chosen = await PickPeople(group.GroupId, group.Name);
            if (chosen is { Count: > 0 })
            {
                bridge.InviteToGroup(group.GroupId, chosen.ToList());
                SetStatus("Приглашения отправлены. Код придёт к ним, когда они войдут.");
            }
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось пригласить: {ex.Message}", StatusKind.Failed);
        }
    }

    private async void OnNewWorkspaceClicked(object sender, RoutedEventArgs e)
    {
        if (PickFolder is not null && await PickFolder() is { Length: > 0 } folder)
        {
            await NewWorkspaceAsync(folder);
        }
    }

    private void OnFolderDragOver(object sender, DragEventArgs e)
    {
        bool canAccept = e.DataView.Contains(StandardDataFormats.StorageItems);
        e.AcceptedOperation = canAccept ? DataPackageOperation.Link : DataPackageOperation.None;
        if (canAccept)
        {
            bool onPage = ReferenceEquals(sender, WorkspacePage) && groupId is not null;
            e.DragUIOverride.Caption = onPage ? "Добавить репозиторий в workspace" : "Сделать workspace из папки";
            WorkspaceDropOverlay.Visibility = onPage ? Visibility.Visible : Visibility.Collapsed;
        }
    }

    // The folder left the page or the drag was called off: the hint goes with it (it stayed over the
    // page when the drag ended anywhere but a drop on it, 2026-10-30).
    private void OnFolderDragLeave(object sender, DragEventArgs e) => WorkspaceDropOverlay.Visibility = Visibility.Collapsed;

    private async void OnListDrop(object sender, DragEventArgs e)
    {
        if (await DroppedFolderAsync(e) is { } folder)
        {
            await NewWorkspaceAsync(folder);
        }
    }

    private async void OnPageDrop(object sender, DragEventArgs e)
    {
        WorkspaceDropOverlay.Visibility = Visibility.Collapsed;
        if (await DroppedFolderAsync(e) is { } folder)
        {
            await BindFolderAsync(folder);
        }
    }

    private async Task<string?> DroppedFolderAsync(DragEventArgs e)
    {
        WorkspaceDropOverlay.Visibility = Visibility.Collapsed;
        if (!e.DataView.Contains(StandardDataFormats.StorageItems))
        {
            return null;
        }
        IReadOnlyList<IStorageItem> dropped = await e.DataView.GetStorageItemsAsync();
        if (dropped.OfType<StorageFolder>().FirstOrDefault() is { Path.Length: > 0 } folder)
        {
            return folder.Path;
        }
        SetStatus("Перетащите папку проекта, а не файл.", StatusKind.Warning);
        return null;
    }

    /// «+ Workspace» / a folder dropped on the list: a group named after the folder, with the folder
    /// as its first repository; then «Пригласить». `files`: what the folder holds, asked when null.
    internal async Task NewWorkspaceAsync(string folder, bool? files = null)
    {
        if (bridge is null || (files ?? await AskFilesAsync(folder)) is not { } asFiles)
        {
            return;
        }

        string name = Path.GetFileName(folder.TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar));
        RustBridge core = bridge;
        try
        {
            GroupSummary created = await Task.Run(() => core.CreateGroup(string.IsNullOrWhiteSpace(name) ? "Workspace" : name));
            Select(created.GroupId);
            await BindFolderAsync(folder, asFiles);
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось создать workspace: {ex.Message}", StatusKind.Failed);
        }
    }

    internal async Task BindFolderAsync()
    {
        if (PickFolder is not null && await PickFolder() is { Length: > 0 } folder)
        {
            await BindFolderAsync(folder);
        }
    }

    /// A folder joins the workspace. Code (a git repository): its main is published at once (UX scenario
    /// 1: «Готово. main опубликован»). Files: Omni sets git up in it and saves its first version (UX
    /// scenario 9). `files`: what the folder holds, asked when null.
    internal async Task BindFolderAsync(string folder, bool? files = null)
    {
        if (groupId is null || (files ?? await AskFilesAsync(folder)) is not { } asFiles)
        {
            return;
        }

        string id = groupId;
        string repo = RepoNameFor(folder);
        if (asFiles)
        {
            // Every file is hashed and stored once for the members: a big folder takes minutes.
            SetStatus($"Сохраняю первую версию {repo}: большие файлы — это может занять несколько минут.", StatusKind.Working);
            await RunAsync(
                core =>
                {
                    core.AddWorkspaceRepo(id, repo, folder, files: true);
                    core.SaveWorkspaceVersion(id, repo, "Первая версия");
                },
                $"Папка {repo} в workspace: у участников будет её последняя версия. Изменили файлы — «Сохранить версию».",
                "привязать папку");
            WorkspacesChanged?.Invoke();
            Refresh();
            return;
        }
        await RunAsync(
            core =>
            {
                core.AddWorkspaceRepo(id, repo, folder);
                WorkspaceRepoState? added = core.GetWorkspaceScreen(id).Repos.FirstOrDefault(r => r.Repo == repo);
                if (added?.LocalBranches.FirstOrDefault(b => b.Branch == "main") is { Published: false })
                {
                    core.PublishWorkspaceBranch(id, repo, "main");
                }
            },
            $"Репозиторий {repo} в workspace: участники видят его main. Остальные ветки — «Опубликовать ветку» или git push omni.",
            "привязать папку");
        WorkspacesChanged?.Invoke();
        Refresh();
    }

    private async Task TakeRepoAsync(string repo)
    {
        if (PickFolder is not null && await PickFolder() is { Length: > 0 } picked)
        {
            await TakeRepoAsync(repo, picked);
        }
    }

    /// «Забрать сюда»: an empty chosen folder takes the repository itself, a folder with files gets a
    /// subfolder named after it.
    internal async Task TakeRepoAsync(string repo, string picked)
    {
        if (groupId is null)
        {
            return;
        }

        string id = groupId;
        string folder = Directory.Exists(picked) && Directory.EnumerateFileSystemEntries(picked).Any() ? Path.Combine(picked, repo) : picked;
        if (bridge is null)
        {
            return;
        }
        RustBridge core = bridge;
        try
        {
            bool files = await Task.Run(() =>
            {
                core.TakeWorkspaceRepo(id, repo, folder);
                return core.GetWorkspaceScreen(id).Repos.Any(r => r.Repo == repo && r.Files);
            });
            SetStatus(files ? $"Забираю {repo} в {folder}: файлы появятся сами." : $"Забираю {repo} в {folder}. Когда код дойдёт, нажмите «Забрать».");
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось забрать: {ex.Message}", StatusKind.Failed);
        }
        signature = string.Empty;
        await LoadAsync();
    }

    /// What the folder holds: a folder without git can only be files; one with git — the person says.
    /// Null: cancelled.
    private async Task<bool?> AskFilesAsync(string folder)
    {
        if (!Directory.Exists(Path.Combine(folder, ".git")))
        {
            return true;
        }
        ContentDialog ask = new()
        {
            XamlRoot = XamlRoot,
            RequestedTheme = Presentation.MainWindowPresentation.ActiveTheme,
            Title = "Что в папке?",
            Content = "В папке git-репозиторий. Это код — или файлы проекта: модели, чертежи, документы?",
            PrimaryButtonText = "Код",
            SecondaryButtonText = "Файлы",
            CloseButtonText = "Отмена",
            DefaultButton = ContentDialogButton.Primary,
        };
        return await ask.ShowAsync() switch
        {
            ContentDialogResult.Primary => false,
            ContentDialogResult.Secondary => true,
            _ => null,
        };
    }

    /// «Сохранить версию» of a files repository.
    internal async Task SaveVersionAsync(string repo, string message, Button? pressed = null, List<string>? give = null)
    {
        if (bridge is null || groupId is null)
        {
            return;
        }

        string id = groupId;
        RustBridge core = bridge;
        // A big folder takes seconds: the button says so and is not pressed twice (the card is redrawn after).
        if (pressed is not null)
        {
            pressed.IsEnabled = false;
            pressed.Content = "Сохраняю…";
        }

        try
        {
            string? saved = await Task.Run(() => core.SaveWorkspaceVersion(id, repo, message));
            if (saved is null) SetStatus("Сохранять нечего: в папке ничего не изменилось.");
            else
            {
                WorkspaceFilesDetails details = await Task.Run(() => core.WorkspaceFiles(new("details", id, repo)));
                bool waiting = details.Proposals.Any(p => p.Oid == saved && !p.Accepted);
                string outcome = waiting ? "Версия сохранена и ждёт принятия админом или хранителем." : "Версия принята. Участники получат её по настройкам загрузки.";
                // The version is saved either way: giving the files back is told apart.
                string? notGiven = null;
                if (give is { Count: > 0 })
                {
                    try { await Task.Run(() => core.WorkspaceFiles(new("give", id, repo, Paths: give))); }
                    catch (Exception ex) { notGiven = ex.Message; }
                }
                if (notGiven is not null) SetStatus($"{outcome} Взятые файлы отдать не удалось: {notGiven}", StatusKind.Warning);
                else SetStatus(outcome + (give is { Count: > 0 } ? $" Отданы: {Names(give)}." : ""));
            }
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось сохранить версию: {ex.Message}", StatusKind.Failed);
        }
        signature = string.Empty;
        await LoadAsync();
    }

    internal async Task TakeFromDeviceAsync(string repo, WorkspaceDeviceBranch theirs)
    {
        if (bridge is null || groupId is null)
        {
            return;
        }

        string id = groupId;
        RustBridge core = bridge;
        try
        {
            string outcome = await Task.Run(() => core.TakeWorkspaceBranchFromDevice(id, repo, theirs.Node, theirs.Branch));
            SetStatus(outcome switch
            {
                "updated" => $"Ветка {theirs.Branch} с «{theirs.Device}» — в папке.",
                "up_to_date" => $"В папке {theirs.Branch} уже не старше, чем на «{theirs.Device}».",
                "dirty" => $"В {theirs.Branch} есть несохранённые изменения — сохраните их и нажмите ещё раз.",
                "diverged" => $"{theirs.Branch} разошлась с «{theirs.Device}» — сведите в IDE.",
                _ => $"Не удалось забрать {theirs.Branch} с «{theirs.Device}».",
            }, outcome is "updated" or "up_to_date" ? StatusKind.Done : outcome is "dirty" or "diverged" ? StatusKind.Warning : StatusKind.Failed);
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось забрать с устройства: {ex.Message}", StatusKind.Failed);
        }
        signature = string.Empty;
        await LoadAsync();
    }

    internal async Task PullAsync(string repo)
    {
        if (bridge is null || groupId is null)
        {
            return;
        }

        string id = groupId;
        RustBridge core = bridge;
        try
        {
            string outcome = await Task.Run(() => core.PullWorkspaceRepo(id, repo));
            SetStatus(outcome switch
            {
                "updated" => $"main в папке {repo} обновлён.",
                "up_to_date" => $"В папке {repo} уже последний main.",
                "dirty" => "В папке есть несохранённые изменения в main — заберите в IDE (git pull omni).",
                "not_on_main" => "В папке открыта другая ветка — новый main забран как omni-shared/main, перейди на main в IDE.",
                "diverged" => "Ваш main ушёл в другую сторону — сведите с omni-shared/main в IDE.",
                _ => "Общего main ещё нет.",
            }, outcome is "updated" or "up_to_date" ? StatusKind.Done : StatusKind.Warning);
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось забрать: {ex.Message}", StatusKind.Failed);
        }
        signature = string.Empty;
        await LoadAsync();
    }

    private async Task RunAsync(Action<RustBridge> action, string done, string what)
    {
        if (bridge is null)
        {
            return;
        }

        RustBridge core = bridge;
        try
        {
            await Task.Run(() => action(core));
            SetStatus(done);
        }
        catch (Exception ex)
        {
            SetStatus($"Не удалось {what}: {ex.Message}", StatusKind.Failed);
        }
        signature = string.Empty;
        await LoadAsync();
    }

    // What came of a button, said by its caller (not guessed from the words): done, still at it, done
    // but something is left to the person, failed.
    private enum StatusKind { Done, Working, Warning, Failed }

    private void SetStatus(string? text, StatusKind kind = StatusKind.Done)
    {
        // What a button did, on the places' strip before the actions: a soft chip of its tone — green
        // when done, red when it failed, amber when the person has something left to do, blue while it
        // is still at it. Done goes by itself (the page already shows the result); the rest stay until
        // closed or replaced. A long one is cut, whole in its tooltip.
        statusText = text ?? string.Empty;
        if (statusText.Length == 0)
        {
            WorkspaceStatusHost.Content = null;
        }
        else
        {
            string tone = kind switch { StatusKind.Failed => "Critical", StatusKind.Warning => "Caution", StatusKind.Working => "Accent", _ => "Success" };
            StackPanel line = new() { Orientation = Orientation.Horizontal, Spacing = 6 };
            TextBlock said = new() { Text = statusText, Style = (Style)Resources[$"WorkspaceChip{tone}TextStyle"], MaxWidth = 460, TextTrimming = TextTrimming.CharacterEllipsis, VerticalAlignment = VerticalAlignment.Center };
            ToolTipService.SetToolTip(said, statusText);
            line.Children.Add(said);
            if (kind != StatusKind.Done)
            {
                Button close = new() { Content = new FontIcon { Glyph = "\uE711", FontSize = 10 }, Padding = new Thickness(4), MinWidth = 0, MinHeight = 0, Background = null, BorderThickness = new Thickness(0), VerticalAlignment = VerticalAlignment.Center };
                Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(close, "Закрыть");
                close.Click += (_, _) => SetStatus(null);
                line.Children.Add(close);
            }
            Border chip = new() { Style = (Style)Resources[$"WorkspaceChip{tone}Style"], Child = line, VerticalAlignment = VerticalAlignment.Center };
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetLiveSetting(chip, Microsoft.UI.Xaml.Automation.Peers.AutomationLiveSetting.Polite);
            WorkspaceStatusHost.Content = chip;
        }
        statusFades ??= DispatcherQueue.CreateTimer();
        statusFades.Stop();
        if (statusText.Length > 0 && kind == StatusKind.Done)
        {
            statusFades.Interval = StatusShownFor;
            statusFades.IsRepeating = false;
            statusFades.Tick -= OnStatusFaded;
            statusFades.Tick += OnStatusFaded;
            statusFades.Start();
        }
    }

    private static readonly TimeSpan StatusShownFor = TimeSpan.FromSeconds(5);
    private Microsoft.UI.Dispatching.DispatcherQueueTimer? statusFades;

    private string statusText = string.Empty;

    private void OnStatusFaded(Microsoft.UI.Dispatching.DispatcherQueueTimer sender, object args) => SetStatus(null);

    private static void OpenFolder(string folder)
    {
        try
        {
            Process.Start(new ProcessStartInfo { FileName = folder, UseShellExecute = true });
        }
        catch (Exception)
        {
            // Explorer refused (the folder moved): the path is on the card to copy by hand.
        }
    }

    /// A repository name the core takes: the folder's name, letters and digits kept, the rest '-'.
    internal static string RepoNameFor(string folder)
    {
        string name = Path.GetFileName(folder.TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar));
        StringBuilder plain = new();
        foreach (char c in name)
        {
            plain.Append(char.IsAsciiLetterOrDigit(c) || c is '-' or '_' or '.' ? c : '-');
        }
        string repo = plain.ToString().Trim('-', '.');
        return repo.Length == 0 ? "code" : repo.Length > 64 ? repo[..64] : repo;
    }

    internal static string CommitsWord(int n)
    {
        int mod100 = n % 100;
        int mod10 = n % 10;
        if (mod100 is >= 11 and <= 14)
        {
            return "коммитов";
        }
        return mod10 switch
        {
            1 => "коммит",
            >= 2 and <= 4 => "коммита",
            _ => "коммитов",
        };
    }

    // What the page shows, as one string: redraw only when it changes (keeps scroll and menus).
    private static string Signature(WorkspaceScreen view)
    {
        StringBuilder s = new($"{view.IAmAdmin}|{view.IAmMaintainer}|{view.IHaveOtherDevices}|{string.Join(',', view.CustodianOf ?? new())}|{string.Join(',', view.CustodyAsked ?? new())}");
        foreach (WorkspaceRepoState r in view.Repos)
        {
            s.Append($"|{r.Repo}:{r.Here}:{r.WorkPath}:{r.Main?.Oid}:{r.NeedsApproval}:{r.MainBehind}:{r.Files}:{r.Version}:{r.ChangedCount}:{r.FileCount}:{r.TotalBytes}");
            s.Append(string.Join("/", r.Changed ?? new()));
            s.Append($"|{r.Own?.Commit.Oid}:{r.Own?.Count}:{r.Own?.State}:{r.FolderCurrent}");
            s.Append($"|{string.Join(",", r.LeftOut ?? new())}|{string.Join(",", r.Taken ?? new())}|{string.Join(",", (r.HeldElsewhere ?? new()).Select(h => h.Path + "@" + h.Name))}|{string.Join(",", r.Conflicts ?? new())}");
            if (r.Sync is { } sync)
            {
                s.Append($"|{string.Join(",", sync.WaitingOpen)}|{string.Join(",", sync.WaitingChanged)}|{sync.ContentMissingBytes}|{sync.Problem}");
            }
            foreach (WorkspaceTip t in r.Waiting.Concat(r.Forks).Concat(r.Branches))
            {
                s.Append($"/{t.AccountId}:{t.Branch}:{t.Oid}:{t.Commit is not null}:{t.Name}");
            }
            foreach (WorkspaceLocalBranch b in r.LocalBranches)
            {
                s.Append($"/{b.Branch}:{b.Oid}:{b.Published}:{b.OnMyDevices}");
            }
            foreach (WorkspaceDeviceBranch d in r.DeviceBranches ?? new())
            {
                s.Append($"/{d.Node}:{d.Branch}:{d.Oid}:{d.State}:{d.Commit is not null}");
            }
        }
        return s.ToString();
    }

    private static string Short(string oid) => oid.Length > 7 ? oid[..7] : oid;

    private static string Shorten(string text, int max) => text.Length <= max ? text : text[..(max - 1)] + "…";

    // The style holds the background as a ThemeResource: a brush read from Application.Resources is
    // the app theme's one, and it stayed dark under a light window.
    private FrameworkElement Card(UIElement content) => new Border
    {
        Style = (Style)Resources["WorkspaceCardStyle"],
        Child = content,
    };

    private static TextBlock Body(string text) => new() { Text = text, TextWrapping = TextWrapping.Wrap };

    // The title of a card and of a project's page: the app's one (App.xaml, CardTitleTextStyle).
    private static Style CardTitle => (Style)Application.Current.Resources["CardTitleTextStyle"];

    // A soft chip of a state (WorkspaceChip<Tone>Style: Success, Caution, Critical, Accent).
    private Border Chip(string text, string tone) => new()
    {
        Style = (Style)Resources[$"WorkspaceChip{tone}Style"],
        Child = new TextBlock { Text = text, Style = (Style)Resources[$"WorkspaceChip{tone}TextStyle"] },
    };

    // A state of a files project: its chip, then what it means.
    private FrameworkElement StateLine(string chip, string tone, string text)
    {
        Grid line = new() { ColumnSpacing = 10 };
        line.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        line.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        line.Children.Add(Chip(chip, tone));
        TextBlock what = new() { Text = text, TextWrapping = TextWrapping.Wrap, VerticalAlignment = VerticalAlignment.Center };
        Grid.SetColumn(what, 1);
        line.Children.Add(what);
        return line;
    }

    private static TextBlock Caption(string text) =>
        new() { Text = text, FontSize = 12, Opacity = 0.7, TextWrapping = TextWrapping.Wrap };

    private static TextBlock SectionLabel(string text) =>
        new() { Text = text, FontSize = 12, FontWeight = Microsoft.UI.Text.FontWeights.SemiBold, Margin = new Thickness(0, 4, 0, 0) };

    // A plain Omni button (the same as «+ Группа»), accent for the one main action of a sentence.
    private static Button PlainButton(string text, bool accent, Action? onClick)
    {
        Button button = new() { Content = text };
        if (accent)
        {
            button.Style = (Style)Application.Current.Resources["AccentActionButtonStyle"];
        }
        if (onClick is not null)
        {
            button.Click += (_, _) => onClick();
        }
        return button;
    }

    private static Button IconButton(string glyph, string text, Action? onClick)
    {
        StackPanel content = new() { Orientation = Orientation.Horizontal, Spacing = 6 };
        content.Children.Add(new FontIcon { Glyph = glyph, FontSize = 14 });
        content.Children.Add(new TextBlock { Text = text });
        Button button = new() { Content = content };
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(button, text);
        if (onClick is not null)
        {
            button.Click += (_, _) => onClick();
        }
        return button;
    }

    // A line with a commit id on the right (selectable, to copy into git).
    private static FrameworkElement Line(string text, string oid)
    {
        Grid line = new() { ColumnSpacing = 10 };
        line.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        line.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        line.Children.Add(new TextBlock { Text = text, TextTrimming = TextTrimming.CharacterEllipsis, VerticalAlignment = VerticalAlignment.Center });
        TextBlock id = new()
        {
            Text = oid,
            FontFamily = new FontFamily("Cascadia Mono, Consolas"),
            FontSize = 12,
            Opacity = 0.6,
            IsTextSelectionEnabled = true,
            VerticalAlignment = VerticalAlignment.Center,
        };
        Grid.SetColumn(id, 1);
        line.Children.Add(id);
        return line;
    }

    // A sentence with one action button: «отстаёт — Забрать», «ждёт одобрения — Одобрить».
    private static FrameworkElement ActionRow(string text, string button, Action onClick)
    {
        Grid row = new() { ColumnSpacing = 12 };
        row.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        row.Children.Add(new TextBlock { Text = text, TextWrapping = TextWrapping.Wrap, VerticalAlignment = VerticalAlignment.Center });
        Button go = PlainButton(button, accent: true, onClick);
        Grid.SetColumn(go, 1);
        row.Children.Add(go);
        return row;
    }

    // UI test channel (docs/decisions/ui-test-channel.md): what the page says, buttons in brackets.
    internal async Task<string> DescribeForUiTestAsync()
    {
        await LoadAsync();
        StringBuilder text = new();
        text.AppendLine($"workspace={group?.Name ?? "-"} list={string.Join(",", items.Select(i => i.Name))}");
        text.AppendLine($"status={statusText}");
        text.AppendLine($"actions={string.Join(", ", WorkspacePlaceActions.Children.OfType<Button>().Select(b => b.Content))}");
        // A button's words, however its content is laid out (a project's row is a grid of lines).
        static IEnumerable<string> Texts(object? element) => element switch
        {
            TextBlock { Text.Length: > 0 } t => [t.Text],
            Panel p => p.Children.SelectMany(c => Texts(c)),
            Border b => Texts(b.Child),
            _ => [],
        };
        void Walk(object? element)
        {
            switch (element)
            {
                // What the person does not see is not on the page.
                case UIElement { Visibility: Visibility.Collapsed }:
                case InfoBar { IsOpen: false }:
                    break;
                case TextBlock t when !string.IsNullOrEmpty(t.Text):
                    text.AppendLine(t.Text);
                    break;
                case ToggleSwitch toggle:
                    text.AppendLine($"<{(toggle.Header is TextBlock header ? header.Text : toggle.Header)}: {(toggle.IsOn ? toggle.OnContent : toggle.OffContent)}>");
                    break;
                case InfoBar bar:
                    text.AppendLine($"! {bar.Title}: {bar.Message}");
                    break;
                case TextBox box:
                    text.AppendLine($"<{box.PlaceholderText}: {box.Text}>");
                    break;
                case Microsoft.UI.Xaml.Controls.Primitives.ToggleButton check:
                    text.AppendLine($"<{(check.IsChecked == true ? "x" : " ")}> {check.Content}");
                    break;
                case Button b:
                    string label = b.Content as string ?? string.Join(" ", Texts(b.Content));
                    text.AppendLine($"[{label}{(b.IsEnabled ? string.Empty : " (неактивна)")}]");
                    break;
                case Panel panel:
                    foreach (UIElement child in panel.Children)
                    {
                        Walk(child);
                    }
                    break;
                case Border border:
                    Walk(border.Child);
                    break;
                case ScrollViewer scroll:
                    Walk(scroll.Content);
                    break;
                case ItemsControl list:
                    foreach (object item in list.Items)
                    {
                        Walk(item is ContentControl row ? row.Content : item);
                    }
                    break;
            }
        }
        Walk(ProjectOpen ? WorkspaceProjectHost : WorkspaceCodePanel);
        // An open dialog of the section («История», «Хранители и правила»): its whole body, scrolled or not.
        if (XamlRoot is not null)
        {
            foreach (Microsoft.UI.Xaml.Controls.Primitives.Popup popup in VisualTreeHelper.GetOpenPopupsForXamlRoot(XamlRoot))
            {
                if (popup.Child is ContentDialog dialog)
                {
                    text.AppendLine($"== {dialog.Title}");
                    Walk(dialog.Content);
                }
            }
        }
        return text.ToString().TrimEnd();
    }

    private void ApplyRailWidth() => WorkspacesListColumn.Width = new GridLength(Shared.SectionRail.Width);
}
