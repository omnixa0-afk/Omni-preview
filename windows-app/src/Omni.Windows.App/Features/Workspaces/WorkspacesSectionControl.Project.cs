using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Threading.Tasks;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Omni.Windows.App.Presentation;
using Windows.ApplicationModel.DataTransfer;

namespace Omni.Windows.App.Features.Workspaces;

/// The page of a files project («Файлы и предложения»), in the section instead of the cards: the
/// files on the left with their own scroll, the chosen file on the right — what it is, its actions, its
/// versions, its preview. Proposals and two edits of one file sit above, compact. It was a dialog: a
/// list scrolling inside a scrolling dialog, the choice lost after every button (review 2026-11-01).
public sealed partial class WorkspacesSectionControl
{
    // Which page is open: every open and the way back to the list moves it, so an answer for a page left is dropped.
    private int projectTicket;
    private string? projectRepo;
    private double cardsOffset;

    // The project page left for the tasks board: «Проекты» brings it back, not the list.
    private string? projectBeforeTasks;
    private bool switching;

    internal bool ProjectOpen => projectRepo is not null;

    private enum Place { Overview, Projects, Tasks, Talk, Agents }

    // The tasks board, the discussions and the agents' place stand in `projectRepo` for the place open.
    private bool OnPlaceOfItsOwn => projectRepo is TasksRepo or TalkPlace or AgentsPlace or OverviewPlace;

    // The switcher follows the place shown, without opening it a second time.
    private void ShowSwitcher(Place place)
    {
        switching = true;
        (place switch { Place.Overview => WorkspaceOverviewTab, Place.Tasks => WorkspaceTasksTab, Place.Talk => WorkspaceTalkTab, Place.Agents => WorkspaceAgentsTab, _ => WorkspaceProjectsTab }).IsSelected = true;
        switching = false;
    }

    private void OnSwitcherChanged(SelectorBar sender, SelectorBarSelectionChangedEventArgs args)
    {
        if (switching) return;
        string? chosen = sender.SelectedItem == WorkspaceOverviewTab ? OverviewPlace
            : sender.SelectedItem == WorkspaceTasksTab ? TasksRepo
            : sender.SelectedItem == WorkspaceTalkTab ? TalkPlace
            : sender.SelectedItem == WorkspaceAgentsTab ? AgentsPlace : null;
        if (chosen is not null)
        {
            if (projectRepo == chosen) return;
            if (!OnPlaceOfItsOwn) projectBeforeTasks = projectRepo;
            _ = chosen switch { OverviewPlace => ShowOverviewAsync(), TasksRepo => ShowTasksAsync(), TalkPlace => ShowTalkAsync(), _ => ShowAgentsAsync() };
        }
        else if (OnPlaceOfItsOwn)
        {
            string? code = codeRepo;
            if (projectBeforeTasks is { } repo) _ = ShowFilesAsync(repo);
            else BackToCards();
            if (code is not null && projectBeforeTasks is null) OpenCode(code);
        }
    }

    /// «Проекты»: the list again, where it was.
    internal void BackToCards()
    {
        projectTicket++;
        projectRepo = null;
        projectBeforeTasks = null;
        projectScreenChanged = null;
        codeRepo = null;
        ShowSwitcher(Place.Projects);
        WorkspaceProjectHost.Children.Clear();
        WorkspaceProjectHost.Visibility = Visibility.Collapsed;
        WorkspaceCardsScroll.Visibility = Visibility.Visible;
        double offset = cardsOffset;
        WorkspaceCardsScroll.UpdateLayout();
        WorkspaceCardsScroll.ChangeView(null, offset, null, disableAnimation: true);
        signature = string.Empty;
        _ = LoadAsync();
    }

    /// «История» of a project: its page on that tab; `pending` — on the revision that waits for нормоконтроль.
    internal Task ShowHistoryAsync(string repo, bool pending = false) => ShowFilesAsync(repo, tab: TabHistory, pendingRevision: pending);

    /// «Настройки» of a project: keepers, the rules, how much history is kept.
    internal Task ShowCustodyAsync(string repo) => ShowFilesAsync(repo, tab: TabSettings);

    private const string TabFiles = "files", TabProposals = "proposals", TabHistory = "history", TabSettings = "settings";

    /// Open a project's page. `select` (a path) and `compare` (в.N): a file's preview beside an older
    /// version; `proposal` (a version id): that proposal looked at, as «Проверить предложение» in
    /// «Требует внимания» asks; `conflict` (a path): its two edits to choose from; `changedOnly`: the
    /// files changed here only; `tab`: which of «Файлы · Предложения · История · Настройки».
    internal async Task ShowFilesAsync(string repo, string? select = null, int compare = 0, string? proposal = null, string? conflict = null, bool changedOnly = false, string tab = TabFiles, bool pendingRevision = false)
    {
        if (bridge is null || groupId is null) return;
        RustBridge core = bridge; string id = groupId;
        if (!ProjectOpen) cardsOffset = WorkspaceCardsScroll.VerticalOffset;
        int page = ++projectTicket;
        projectRepo = repo;
        projectBeforeTasks = null;
        codeRepo = null;
        ShowSwitcher(Place.Projects);
        bool Live() => page == projectTicket;

        // What the person is at: kept through every reload, so a button's result never loses it.
        string? inspected = proposal;
        string? selectedPath = select;
        string? selectedConflict = conflict;
        int compareWith = compare;
        bool onlyChanged = changedOnly;
        bool allVersions = false;
        bool narrowShowsDetails = select is not null || conflict is not null;
        int previewTicket = 0;
        // Every read of the page is numbered: only the last one asked is drawn, and only for the
        // version it was asked for (a proposal looked at, then «К общей версии» before it answered).
        int reloadTicket = 0;
        // One command at a time: a second press while the first is under way is not sent.
        bool busy = false;
        // The variant of two edits chosen, and «Снять замок?» asked: kept through a redraw.
        string? chosenVariant = null;
        string? askingUnlock = null;
        bool allProposals = false;
        // A version opened from «История»: what its banner says, and which row the history returns to.
        string? inspectedLabel = null, historyChosen = null;
        string shownTab = proposal is not null ? TabProposals : tab;
        // A proposal shows only what it changes until «Все файлы» is asked for.
        bool allProposalFiles = false;
        // The first look at the page picks a file, so the right side is never an empty instruction.
        bool pickFirst = select is null && conflict is null;
        WorkspaceFilesDetails? view = null;

        Grid frame = new() { Padding = UnscrolledPadding };
        frame.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        frame.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        frame.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        frame.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        frame.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });

        // The project's doors, on the places' strip — the folder on this computer, the
        // rarer ones under «Ещё». The page follows the workspace's read by itself — no «Обновить».
        StackPanel doors = new() { Orientation = Orientation.Horizontal, Spacing = 8 };
        Button openFolder = IconButton("", "Открыть папку", () => { if (view?.WorkPath is { } folder) OpenFolder(folder); });
        openFolder.Visibility = Visibility.Collapsed;
        doors.Children.Add(openFolder);
        Button more = IconButton("", "Ещё", null);
        MenuFlyout moreMenu = new() { Placement = Microsoft.UI.Xaml.Controls.Primitives.FlyoutPlacementMode.Bottom };
        more.Flyout = moreMenu;
        doors.Children.Add(more);
        // «На этом устройстве»: which of the project's folders come into the working folder here.
        MenuFlyoutItem device = new() { Text = "На этом устройстве…" };
        void DrawMore()
        {
            moreMenu.Items.Clear();
            if (inspected is null && view?.WorkPath is not null && view.Folders is { Count: > 0 }) moreMenu.Items.Add(device);
            MenuFlyoutItem place = view?.WorkPath is null ? new() { Text = "Забрать проект в папку…" } : new() { Text = "Убрать папку с этого компьютера…" };
            place.Click += (_, _) =>
            {
                if (screen?.Repos.FirstOrDefault(r => r.Repo == repo) is not { } state) return;
                _ = state.WorkPath is null ? TakeRepoAsync(repo) : UnbindFolderAsync(state);
            };
            moreMenu.Items.Add(place);
        }

        // What the last button did: on the places' strip, never scrolled away from.
        void Say(string text, StatusKind kind = StatusKind.Done)
        {
            if (Live()) SetStatus(text, kind);
        }

        // Above the files: what is in the way and the person's own changes with «Сохранить версию»
        // (from the workspace's read), then others' versions waiting.
        // One compact group of reading width: the changes, «Показать», what changed and «Сохранить версию»
        // close together, not spread over the whole page.
        StackPanel above = new() { Spacing = 12, Margin = new Thickness(0, 12, 0, 0), Visibility = Visibility.Collapsed };
        StackPanel work = new();
        above.Children.Add(work);
        Grid aboveFrame = Bounded(above, ColumnMax);
        Grid.SetRow(aboveFrame, 2);
        frame.Children.Add(aboveFrame);

        // The project's own tabs: segments, not the section's places — two levels that do not look alike.
        Action<string> chooseTab = _ => { };
        (FrameworkElement tabs, Action<string> markTab, Action<string, string> renameTab) = Segments(
            [(TabFiles, "Файлы"), (TabProposals, "Предложения"), (TabHistory, "История"), (TabSettings, "Настройки")],
            tag => chooseTab(tag));

        // Under the tabs: the files (also a proposal's files), a proposal's decision above them, the
        // history, the settings — one at a time.
        Grid place = new();
        place.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        place.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
        Grid.SetRow(place, 4);
        frame.Children.Add(place);
        StackPanel proposals = new() { Spacing = 8 };
        place.Children.Add(proposals);
        Grid historyHost = new();
        Grid.SetRow(historyHost, 1);
        place.Children.Add(historyHost);
        // The settings are one column of rows, their switches and buttons on one right edge.
        StackPanel settingsBody = new() { Spacing = 8 };
        ScrollViewer settingsScroll = new() { Content = Bounded(settingsBody, ColumnMax), VerticalScrollBarVisibility = ScrollBarVisibility.Auto, Padding = new Thickness(0, 0, 12, 0) };
        Grid.SetRow(settingsScroll, 1);
        place.Children.Add(settingsScroll);
        SaveDraft draft = new();
        // Our version was turned down: its files say so, not «waiting».
        bool OwnRejected() => screen?.Repos.FirstOrDefault(r => r.Repo == repo)?.Own is { State: "rejected" };
        void DrawMine(WorkspaceScreen from)
        {
            work.Children.Clear();
            if (from.Repos.FirstOrDefault(r => r.Repo == repo) is { } state)
            {
                StackPanel mine = FilesWork(state, () => { onlyChanged = true; DrawList(); }, draft);
                if (mine.Children.Count > 0) work.Children.Add(mine);
            }
            above.Visibility = shownTab == TabFiles && work.Children.Count > 0 ? Visibility.Visible : Visibility.Collapsed;
        }

        Grid panes = new() { ColumnSpacing = 24 };
        // The list at the person's width (its edge is theirs to drag); the file takes the rest.
        panes.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(Features.Shared.ColumnWidth.PageList.Width) });
        panes.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(3, GridUnitType.Star) });
        Grid.SetRow(panes, 1);
        place.Children.Add(panes);

        // Which place shows: a tab chosen, or the page opened on one.
        void ShowTab()
        {
            markTab(shownTab);
            bool files = shownTab == TabFiles, looking = shownTab == TabProposals;
            proposals.Visibility = looking || inspected is not null ? Visibility.Visible : Visibility.Collapsed;
            panes.Visibility = files || (looking && inspected is not null) ? Visibility.Visible : Visibility.Collapsed;
            historyHost.Visibility = shownTab == TabHistory ? Visibility.Visible : Visibility.Collapsed;
            above.Visibility = files && work.Children.Count > 0 ? Visibility.Visible : Visibility.Collapsed;
            settingsScroll.Visibility = shownTab == TabSettings ? Visibility.Visible : Visibility.Collapsed;
        }

        Grid left = new() { RowSpacing = 8 };
        left.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        left.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        left.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
        left.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        panes.Children.Add(left);
        StackPanel conflictsPanel = new() { Spacing = 8 };
        left.Children.Add(conflictsPanel);
        CheckBox onlyChangedBox = new() { Visibility = Visibility.Collapsed };
        Grid.SetRow(onlyChangedBox, 1);
        left.Children.Add(onlyChangedBox);
        ListView tree = new() { SelectionMode = ListViewSelectionMode.Single };
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(tree, $"Файлы проекта {repo}");
        Grid.SetRow(tree, 2);
        left.Children.Add(tree);
        ToolTipService.SetToolTip(tree, "Файл можно перетащить в Проводник — там окажется его копия");

        StackPanel details = new() { Spacing = 12, Padding = new Thickness(0, 0, 12, 0) };
        ScrollViewer right = new() { Content = details, VerticalScrollBarVisibility = ScrollBarVisibility.Auto };
        Grid.SetColumn(right, 1);
        panes.Children.Add(right);

        // A narrow window: the list, or the file with «К списку» — two columns would squeeze both.
        bool narrow = false;
        void Arrange()
        {
            bool was = narrow;
            narrow = panes.ActualWidth is > 0 and < FilesTwoPanesFrom;
            panes.ColumnDefinitions[0].Width = narrow ? new GridLength(1, GridUnitType.Star) : new GridLength(Features.Shared.ColumnWidth.PageList.Width);
            panes.ColumnDefinitions[1].Width = narrow ? new GridLength(0) : new GridLength(3, GridUnitType.Star);
            Grid.SetColumn(right, narrow ? 0 : 1);
            left.Visibility = narrow && narrowShowsDetails ? Visibility.Collapsed : Visibility.Visible;
            right.Visibility = narrow && !narrowShowsDetails ? Visibility.Collapsed : Visibility.Visible;
            // «К списку» belongs to the narrow layout only.
            if (was != narrow && view is not null) DrawDetails();
        }
        panes.SizeChanged += (_, _) => Arrange();
        panes.Children.Add(new Features.Shared.RailSplitter { Source = Features.Shared.ColumnWidth.PageList });
        void FollowWidth()
        {
            if (!Live()) { Features.Shared.ColumnWidth.PageList.Changed -= FollowWidth; return; }
            Arrange();
        }
        Features.Shared.ColumnWidth.PageList.Changed += FollowWidth;
        void ShowDetails()
        {
            narrowShowsDetails = true;
            Arrange();
            right.ChangeView(null, 0, null, disableAnimation: true);
        }

        WorkspaceCardsScroll.Visibility = Visibility.Collapsed;
        WorkspaceProjectHost.Children.Clear();
        SetPlaceBar(doors);
        SetPlaceTitle(repo, tabs);
        WorkspaceProjectHost.Children.Add(frame);
        WorkspaceProjectHost.Visibility = Visibility.Visible;
        // The workspace's read (every 2 s) brings the folder's changes: the strip and the files follow.
        projectScreenChanged = fresh =>
        {
            if (!Live()) return;
            DrawMine(fresh);
            _ = ReloadSafe();
        };
        if (screen is not null) DrawMine(screen);

        async Task Reload()
        {
            int ask = ++reloadTicket;
            string? forVersion = inspected;
            await WorkspaceFolderPreferences.EnsureAsync(core);
            WorkspaceFilesDetails fresh = await Task.Run(() => core.WorkspaceFiles(new("details", id, repo, Oid: forVersion)));
            if (!Live() || ask != reloadTicket || forVersion != inspected) return;
            view = fresh;
            int waiting = view.Proposals.Count(p => !p.Accepted && !p.Rejected);
            renameTab(TabProposals, waiting > 0 ? $"Предложения · {waiting}" : "Предложения");
            if (pickFirst && selectedPath is null && selectedConflict is null)
            {
                List<string> changedHere = inspected is not null && view.Proposals.FirstOrDefault(p => p.Oid == inspected) is { } looked
                    ? looked.Changed : view.Files.Where(IsMine).Select(f => f.Path).ToList();
                selectedPath = changedHere.FirstOrDefault(c => view.Files.Any(f => f.Path == c)) ?? view.Files.FirstOrDefault()?.Path;
            }
            pickFirst = false;
            ShowTab();
            openFolder.Visibility = view.WorkPath is not null ? Visibility.Visible : Visibility.Collapsed;
            DrawMore();
            // The keyboard stays where it was: on the chosen row when the list had it.
            bool listFocused = XamlRoot is not null && Microsoft.UI.Xaml.Input.FocusManager.GetFocusedElement(XamlRoot) is ListViewItem focusedRow && tree.Items.Contains(focusedRow);
            DrawProposals();
            DrawConflictList();
            DrawList();
            DrawDetails();
            if (tree.SelectedItem is ListViewItem chosenRow)
            {
                tree.ScrollIntoView(chosenRow);
                if (listFocused) chosenRow.Focus(FocusState.Programmatic);
            }
        }
        async Task ReloadSafe()
        {
            try { await Reload(); }
            catch (Exception ex) { Say(ex.Message, StatusKind.Failed); }
        }
        // A command of the page; the page is read again after it, the file chosen stays chosen. A
        // press while another command is under way is dropped.
        async Task<bool> Act(WorkspaceFilesRequest request, bool leaveProposal = false)
        {
            if (busy) return false;
            busy = true;
            try
            {
                await Task.Run(() => core.WorkspaceFiles(request));
            }
            catch (Exception ex) { Say(ex.Message, StatusKind.Failed); return false; }
            finally { busy = false; }
            if (leaveProposal) SwitchVersion(null);
            try { await Reload(); }
            catch (Exception ex) { Say($"Сделано, но страницу не удалось обновить: {ex.Message}", StatusKind.Warning); }
            return true;
        }
        // «Файлы в.N» from the history: that version's files on «Файлы».
        void ShowVersion(string oid, string label)
        {
            historyChosen = oid;
            SwitchVersion(oid);
            inspectedLabel = label;
            pickFirst = true;
            shownTab = TabFiles;
            ShowTab();
            _ = ReloadSafe();
        }
        // Another version looked at: what was drawn for the old one goes at once.
        void SwitchVersion(string? oid)
        {
            inspected = oid;
            selectedPath = null; selectedConflict = null; askingUnlock = null;
            previewTicket++;
            details.Children.Clear();
        }
        device.Click += (_, _) =>
        {
            selectedPath = null; selectedConflict = null; askingUnlock = null;
            tree.SelectedItem = null;
            DrawDetails(); ShowDetails();
        };

        // Others' versions waiting: one line each above the panes; the one looked at — its whole
        // decision, so «Принять» is never below the files.
        void DrawProposals()
        {
            proposals.Children.Clear();
            if (view is null) return;
            if (inspected is not null && view.Proposals.All(p => p.Oid != inspected))
            {
                // A version from «История» (нормоконтроль looks at what it approves): its files, read-only.
                Grid line = new() { ColumnSpacing = 10 };
                line.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
                line.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
                line.Children.Add(new TextBlock { Text = inspectedLabel ?? "Файлы этой версии", FontWeight = Microsoft.UI.Text.FontWeights.SemiBold, VerticalAlignment = VerticalAlignment.Center, TextWrapping = TextWrapping.Wrap });
                Button back = PlainButton("К истории", false, () => { SwitchVersion(null); shownTab = TabHistory; ShowTab(); _ = FillHistoryAsync(repo, historyHost, false, Live, ShowVersion, historyChosen); });
                Grid.SetColumn(back, 1);
                line.Children.Add(back);
                proposals.Children.Add(Card(line));
                return;
            }
            if (inspected is not null && view.Proposals.FirstOrDefault(p => p.Oid == inspected) is { } shown)
            {
                StackPanel card = new() { Spacing = 8 };
                card.Children.Add(new TextBlock { Text = $"{shown.Name} · «{shown.Subject}»", FontWeight = Microsoft.UI.Text.FontWeights.SemiBold, TextWrapping = TextWrapping.Wrap });
                List<string> changed = shown.Changed;
                card.Children.Add(Caption((shown.Rejected ? "Отклонено · " : shown.Accepted ? "Принято · " : "")
                    + $"изменено файлов: {changed.Count}" + (shown.Stale ? " · основано на старой версии: разошедшиеся правки останутся обе, итоговую выберете потом" : "")));
                StackPanel buttons = new() { Orientation = Orientation.Horizontal, Spacing = 8 };
                if (view.CanReview && !shown.Accepted)
                {
                    buttons.Children.Add(PlainButton("Принять версию целиком", true, async () =>
                    {
                        if (await Act(new("review", id, repo, Accepted: true, Oid: shown.Oid), leaveProposal: true))
                            Say($"Версия {shown.Name} принята — папки участников обновятся.");
                    }));
                    buttons.Children.Add(PlainButton("Отклонить", false, async () =>
                    {
                        if (await Act(new("review", id, repo, Accepted: false, Oid: shown.Oid), leaveProposal: true))
                            Say($"Версия {shown.Name} отклонена. Автор увидит это у себя.");
                    }));
                }
                buttons.Children.Add(PlainButton("К списку предложений", false, () => { SwitchVersion(null); _ = ReloadSafe(); }));
                card.Children.Add(buttons);
                proposals.Children.Add(Card(card));
                return;
            }
            // Waiting ones only: a turned-down version is its author's to fix, not a decision here.
            List<WorkspaceProposal> open = view.Proposals.Where(p => !p.Accepted && !p.Rejected).ToList();
            if (open.Count == 0) proposals.Children.Add(Caption("Предложений нет: все версии участников приняты или отклонены."));
            foreach (WorkspaceProposal p in allProposals ? open : open.Take(3))
            {
                Grid line = new() { ColumnSpacing = 10 };
                line.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
                line.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
                line.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
                Border chip = Chip("Ждёт решения", "Accent");
                chip.VerticalAlignment = VerticalAlignment.Center;
                line.Children.Add(chip);
                TextBlock what = new() { Text = $"{p.Name} · «{p.Subject}» · изменено: {p.Changed.Count} — " + string.Join(", ", p.Changed.Take(3).Select(Path.GetFileName)) + (p.Changed.Count > 3 ? $" и ещё {p.Changed.Count - 3}" : ""), TextWrapping = TextWrapping.Wrap, VerticalAlignment = VerticalAlignment.Center };
                Grid.SetColumn(what, 1);
                line.Children.Add(what);
                if (view.CanReview)
                {
                    Button look = PlainButton("Проверить", false, () => { SwitchVersion(p.Oid); pickFirst = true; _ = ReloadSafe(); });
                    Grid.SetColumn(look, 2);
                    line.Children.Add(look);
                }
                proposals.Children.Add(line);
            }
            if (open.Count > 3 && !allProposals)
                proposals.Children.Add(PlainButton($"Ещё предложения ({open.Count - 3})", false, () => { allProposals = true; DrawProposals(); }));
        }

        // Two edits of one file: a line each above the list; the choice itself is on the right.
        void DrawConflictList()
        {
            conflictsPanel.Children.Clear();
            if (view is null || inspected is not null) return;
            foreach (WorkspaceConflict c in view.Conflicts ?? new())
            {
                Grid line = new() { ColumnSpacing = 8 };
                line.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
                line.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
                line.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
                Border chip = Chip("Расхождение", "Caution");
                chip.VerticalAlignment = VerticalAlignment.Center;
                line.Children.Add(chip);
                TextBlock name = new() { Text = c.Path, TextTrimming = TextTrimming.CharacterEllipsis, VerticalAlignment = VerticalAlignment.Center };
                ToolTipService.SetToolTip(name, c.Path);
                Grid.SetColumn(name, 1);
                line.Children.Add(name);
                Button choose = PlainButton("Выбрать…", false, () =>
                {
                    selectedConflict = c.Path; selectedPath = null; chosenVariant = null; askingUnlock = null;
                    tree.SelectedItem = null;
                    DrawDetails(); ShowDetails();
                });
                Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(choose, $"Выбрать итоговую правку {c.Path}");
                Grid.SetColumn(choose, 2);
                line.Children.Add(choose);
                conflictsPanel.Children.Add(line);
            }
        }

        bool drawingList = false;
        void DrawList()
        {
            if (view is null) return;
            int changedCount = view.Files.Count(IsMine);
            List<string>? proposalChanged = inspected is not null ? view.Proposals.FirstOrDefault(p => p.Oid == inspected)?.Changed : null;
            onlyChangedBox.Content = proposalChanged is not null ? $"Все файлы версии ({view.Files.Count})" : $"Только мои изменения ({changedCount})";
            onlyChangedBox.IsChecked = proposalChanged is not null ? allProposalFiles : onlyChanged;
            onlyChangedBox.Visibility = proposalChanged is not null || changedCount > 0 ? Visibility.Visible : Visibility.Collapsed;
            if (changedCount == 0) onlyChanged = false;
            if (proposalChanged is null) onlyChangedBox.IsChecked = onlyChanged;
            drawingList = true;
            tree.Items.Clear();
            foreach (WorkspaceFile file in view.Files.Where(f => proposalChanged is not null ? allProposalFiles || proposalChanged.Contains(f.Path) : !onlyChanged || IsMine(f)))
            {
                ListViewItem item = new() { Content = FileRow(file, proposalChanged?.Contains(file.Path) == true, OwnRejected()), Tag = file, HorizontalContentAlignment = HorizontalAlignment.Stretch };
                Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(item, FileRowName(file));
                // Dragged out to Explorer or the desktop: a copy, never a move out of the project.
                if (file.InVersion)
                {
                    item.CanDrag = true;
                    item.DragStarting += async (_, e) =>
                    {
                        DragOperationDeferral wait = e.GetDeferral();
                        try
                        {
                            WorkspacePreview? ready = (await Task.Run(() => core.WorkspaceFiles(new("export", id, repo, Path: file.Path, Oid: inspected)))).Export;
                            if (ready?.File is { } path)
                            {
                                e.Data.SetStorageItems([await global::Windows.Storage.StorageFile.GetFileFromPathAsync(path)], readOnly: true);
                                e.AllowedOperations = DataPackageOperation.Copy;
                            }
                            else
                            {
                                e.Cancel = true;
                                Say($"Содержимого {file.Path} здесь ещё нет — забираю у участников. Перетащите ещё раз через минуту.", StatusKind.Warning);
                            }
                        }
                        catch (Exception ex) { e.Cancel = true; Say(ex.Message, StatusKind.Failed); }
                        finally { wait.Complete(); }
                    };
                }
                tree.Items.Add(item);
                if (file.Path == selectedPath) tree.SelectedItem = item;
            }
            drawingList = false;
            // The chosen file is gone (removed by the new version, filtered out): say so on the right.
            if (selectedPath is not null && tree.SelectedItem is null && !view.Files.Any(f => f.Path == selectedPath))
            {
                Say($"{selectedPath} больше нет в этой версии.", StatusKind.Warning);
                selectedPath = null;
            }
        }
        static bool IsMine(WorkspaceFile f) => f.State != "accepted" || !f.InVersion;
        onlyChangedBox.Click += (_, _) =>
        {
            if (inspected is not null) allProposalFiles = onlyChangedBox.IsChecked == true;
            else onlyChanged = onlyChangedBox.IsChecked == true;
            DrawList();
        };
        tree.SelectionChanged += (_, _) =>
        {
            if (drawingList || (tree.SelectedItem as ListViewItem)?.Tag is not WorkspaceFile file) return;
            selectedPath = file.Path; selectedConflict = null; compareWith = 0; allVersions = false; askingUnlock = null;
            DrawDetails(); ShowDetails();
        };

        void DrawDetails()
        {
            details.Children.Clear();
            previewTicket++;
            if (view is null) return;
            if (narrow) details.Children.Add(IconButton("", "К списку", () => { narrowShowsDetails = false; Arrange(); }));
            if (selectedConflict is not null && view.Conflicts?.FirstOrDefault(c => c.Path == selectedConflict) is { } conflict)
            {
                DrawConflict(conflict);
                return;
            }
            if (view.Files.FirstOrDefault(f => f.Path == selectedPath) is not { } file)
            {
                if (view.Files.Count == 0)
                    details.Children.Add(Body(inspected is not null ? "В этой версии нет файлов."
                        : screen?.Repos.FirstOrDefault(r => r.Repo == repo) is { ChangedCount: > 0 }
                            ? "В общей версии пока нет файлов. Сохраните версию выше — файлы из вашей папки станут ею."
                            : "В проекте пока нет файлов: положите их в папку проекта и сохраните версию."));
                if (!string.IsNullOrEmpty(view.DownloadStatus))
                {
                    details.Children.Add(Body(view.DownloadStatus));
                    if (view.DownloadStatus.StartsWith("Ожидает", StringComparison.Ordinal))
                        details.Children.Add(PlainButton("Отменить загрузку", false, () => _ = Act(new("cancel_download", id, repo))));
                }
                if (inspected is null && view.WorkPath is not null && view.Folders is { Count: > 0 } folders)
                    DrawLeftOut(folders);
                return;
            }
            details.Children.Add(new TextBlock { Text = file.Path, Style = (Style)Application.Current.Resources["CardTitleTextStyle"], TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true });
            bool looking = inspected is not null && view.Proposals.Any(p => p.Oid == inspected);
            List<string> facts = [Megabytes(file.Bytes)];
            if (file.LeftOut) facts.Add("не забирается на это устройство");
            else if (!file.AvailableHere) facts.Add("содержимое у участников");
            if (!file.InVersion) facts.Add("только у вас: в общей версии его нет");
            else if (file.State == "changed" && inspected is null) facts.Add("есть изменения — уйдут с «Сохранить версию»");
            else if (file.State == "saved" && inspected is null) facts.Add(OwnRejected() ? "ваша версия не принята" : "ваша версия ждёт принятия");
            details.Children.Add(Caption(string.Join(" · ", facts)));
            // The work on the file first («Беру в работу» — the main button until it is taken), then
            // opening it (the main one once it is mine), the picture, its versions.
            // Its state above; the work's buttons and the file's own in one row, 8 apart.
            StackPanel work = new() { Spacing = 8 };
            details.Children.Add(work);
            StackPanel workButtons = new() { Orientation = Orientation.Horizontal, Spacing = 8 };
            DrawWork(work, workButtons, file);
            StackPanel actions = new() { Orientation = Orientation.Horizontal, Spacing = 8 };
            if (file.InVersion)
            {
                actions.Children.Add(PlainButton(inspected is null ? "Открыть" : looking ? "Открыть из предложения" : "Открыть эту версию", file.Lock is { Mine: true }, () => _ = OpenFile(file.Path, inspected, inspected is null ? "" : "предложенную версию ")));
                if (inspected is not null) actions.Children.Add(PlainButton("Открыть общую", false, () => _ = OpenFile(file.Path, null, "")));
                actions.Children.Add(PlainButton(file.AvailableHere || inspected is not null ? "Сохранить копию…" : "Скачать…", false, () => _ = Download(file)));
            }
            StackPanel buttonsRow = new() { Orientation = Orientation.Horizontal, Spacing = 8 };
            buttonsRow.Children.Add(workButtons);
            buttonsRow.Children.Add(actions);
            details.Children.Add(buttonsRow);
            StackPanel preview = new() { Spacing = 8 };
            details.Children.Add(preview);
            StackPanel versions = new() { Spacing = 8 };
            details.Children.Add(versions);
            _ = DrawVersionsAndPreview(file, versions, preview);
        }

        // The file's accepted versions, newest first: five, then all on request — each with its own
        // «Открыть» (an older one opens as a read-only copy). Then the picture, beside an older one on choice.
        async Task DrawVersionsAndPreview(WorkspaceFile file, StackPanel versions, StackPanel preview)
        {
            int ticket = previewTicket;
            if (!file.InVersion) return;
            List<WorkspaceFileVersion> history;
            try { history = (await Task.Run(() => core.WorkspaceFiles(new("file_history", id, repo, Path: file.Path)))).FileHistory ?? []; }
            catch (Exception ex)
            {
                if (ticket == previewTicket && Live()) Say(ex.Message, StatusKind.Failed);
                return;
            }
            if (ticket != previewTicket || !Live()) return;
            if (history.Count > 0)
            {
                versions.Children.Add(SectionLabel("Версии файла"));
                int shown = allVersions ? history.Count : Math.Min(5, history.Count);
                for (int i = 0; i < shown; i++)
                {
                    WorkspaceFileVersion entry = history[i];
                    Grid line = new() { ColumnSpacing = 12 };
                    line.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
                    line.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
                    StackPanel text = new() { Spacing = 2, VerticalAlignment = VerticalAlignment.Center };
                    text.Children.Add(new TextBlock { Text = $"в.{entry.Number} · «{entry.Subject}»" + (i == 0 && inspected is null ? " — сейчас" : ""), TextTrimming = TextTrimming.CharacterEllipsis });
                    text.Children.Add(Caption($"{entry.Author} · {MainWindowPresentation.FormatMessageTime(entry.AtUnixMs)} · "
                        + (entry.Bytes is { } bytes ? Megabytes(bytes) : "файл удалён в этой версии") + (entry.Revision is { } label ? $" · ревизия {label}" + (entry.RevisionPending ? " (на нормоконтроле)" : "") : "")));
                    line.Children.Add(text);
                    // The newest is the file itself («Открыть» above). An older one: beside it in the
                    // picture when Omni can draw the file, else a read-only copy in its program.
                    if (entry.Bytes is not null && (i > 0 || inspected is not null))
                    {
                        bool drawn = inspected is null && Previewable(file.Path);
                        int number = entry.Number;
                        Button opener = drawn
                            ? PlainButton(compareWith == number ? "Сравнивается" : "Сравнить", false, () => { compareWith = number; DrawDetails(); })
                            : PlainButton("Открыть", false, () => _ = OpenFile(file.Path, entry.Oid, $"в.{number} "));
                        opener.IsEnabled = !(drawn && compareWith == number);
                        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(opener, drawn ? $"Сравнить в.{number} файла {file.Path}" : $"Открыть в.{number} файла {file.Path}");
                        if (!drawn) ToolTipService.SetToolTip(opener, "Копия только для чтения");
                        Grid.SetColumn(opener, 1);
                        line.Children.Add(opener);
                    }
                    versions.Children.Add(line);
                }
                if (history.Count > shown)
                    versions.Children.Add(PlainButton($"Все версии ({history.Count})", false, () => { allVersions = true; DrawDetails(); }));
            }
            if (file.Path.EndsWith(".stl", StringComparison.OrdinalIgnoreCase))
            {
                preview.Children.Add(SectionLabel("3D-просмотр"));
                Border model = new();
                preview.Children.Add(model);
                await ShowModel(model, () => ticket == previewTicket, file.Path, inspected);
                return;
            }
            if (!Previewable(file.Path))
            {
                // A model or another document: the picture Windows gets from its program, when one is installed.
                if (inspected is null && file.AvailableHere) await ShowThumbnail(preview, () => ticket == previewTicket, file.Path);
                return;
            }
            List<WorkspaceFileVersion> earlier = inspected is null ? history.Skip(1).Where(h => h.Bytes is not null).ToList() : [];
            WorkspaceFileVersion? older = compareWith > 0 ? earlier.FirstOrDefault(h => h.Number == compareWith) : null;
            if (older is not null)
            {
                StackPanel comparing = new() { Orientation = Orientation.Horizontal, Spacing = 8 };
                comparing.Children.Add(PlainButton("Завершить сравнение", false, () => { compareWith = 0; DrawDetails(); }));
                comparing.Children.Add(PlainButton($"Открыть в.{older.Number}", false, () => _ = OpenFile(file.Path, older.Oid, $"в.{older.Number} ")));
                preview.Children.Add(comparing);
            }
            string nowLabel = inspected is not null ? $"Предложение · {view?.Proposals.FirstOrDefault(p => p.Oid == inspected)?.Name}" : history.Count > 0 ? $"Общая · в.{history[0].Number}" : "Общая версия";
            // A proposal's file beside the shared one it changes; a new file has no shared side.
            if (inspected is not null && view?.Proposals.FirstOrDefault(p => p.Oid == inspected)?.Changed.Contains(file.Path) == true)
            {
                Grid sides = new() { ColumnSpacing = 12 };
                sides.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
                sides.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
                Border shared = new(), proposed = new();
                Grid.SetColumn(proposed, 1);
                sides.Children.Add(shared); sides.Children.Add(proposed);
                preview.Children.Add(sides);
                if (history.Count > 0) _ = ShowPane(shared, () => ticket == previewTicket, file.Path, null, $"Общая · в.{history[0].Number}", 300);
                else shared.Child = Caption("В общей версии этого файла нет — он новый.");
                await ShowPane(proposed, () => ticket == previewTicket, file.Path, inspected, nowLabel, 300);
                return;
            }
            if (older is null)
            {
                Border single = new();
                preview.Children.Add(single);
                await ShowPane(single, () => ticket == previewTicket, file.Path, inspected, nowLabel, 420);
                return;
            }
            Grid pair = new() { ColumnSpacing = 12 };
            pair.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            pair.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            Border was = new(), now = new();
            Grid.SetColumn(now, 1);
            pair.Children.Add(was); pair.Children.Add(now);
            preview.Children.Add(pair);
            _ = ShowPane(was, () => ticket == previewTicket, file.Path, older.Oid, $"в.{older.Number} · {older.Author} · {MainWindowPresentation.FormatMessageTime(older.AtUnixMs)}", 300);
            await ShowPane(now, () => ticket == previewTicket, file.Path, inspected, nowLabel, 300);
        }

        // An STL drawn by the core, turned by the buttons or by dragging across it (drawn again when the
        // drag ends: a model of a million faces takes a moment).
        async Task ShowModel(Border cell, Func<bool> wanted, string path, string? oid)
        {
            float yaw = 35, pitch = 25;
            Image picture = new() { Stretch = Microsoft.UI.Xaml.Media.Stretch.None, HorizontalAlignment = HorizontalAlignment.Left };
            TextBlock note = Caption("Загружаю модель…");
            StackPanel pane = new() { Spacing = 8 };
            Border frame = new()
            {
                BorderThickness = new Thickness(1),
                BorderBrush = MainWindowPresentation.ResolveThemeBrush("OmniPopupStrokeBrush", new Microsoft.UI.Xaml.Media.SolidColorBrush(Microsoft.UI.Colors.Gray)),
                HorizontalAlignment = HorizontalAlignment.Left,
                Child = picture,
                Background = new Microsoft.UI.Xaml.Media.SolidColorBrush(Microsoft.UI.Colors.Transparent),
            };
            StackPanel turns = new() { Orientation = Orientation.Horizontal, Spacing = 8 };
            pane.Children.Add(frame); pane.Children.Add(turns); pane.Children.Add(note);
            cell.Child = pane;
            bool drawing = false, again = false;
            async Task Draw()
            {
                if (drawing) { again = true; return; }
                drawing = true;
                try
                {
                    for (int attempt = 0; attempt < 30 && wanted() && Live(); attempt++)
                    {
                        (float y, float p) = (yaw, pitch);
                        WorkspaceModelView? shown = (await Task.Run(() => core.WorkspaceFiles(new("model_view", id, repo, Path: path, Oid: oid, Yaw: y, Pitch: p, Width: 560, Height: 400)))).ModelView;
                        if (!wanted() || !Live()) return;
                        if (shown?.PngBase64 is { } png)
                        {
                            using global::Windows.Storage.Streams.InMemoryRandomAccessStream stream = new();
                            await stream.WriteAsync(System.Runtime.InteropServices.WindowsRuntime.WindowsRuntimeBufferExtensions.AsBuffer(Convert.FromBase64String(png)));
                            stream.Seek(0);
                            Microsoft.UI.Xaml.Media.Imaging.BitmapImage bitmap = new();
                            await bitmap.SetSourceAsync(stream);
                            picture.Source = bitmap;
                            note.Text = $"Граней: {shown.Triangles:N0}. Поверните кнопками или протащите по модели.";
                            if (!again) return;
                            again = false;
                            attempt = -1;
                            continue;
                        }
                        if (shown?.Pending != true) break;
                        note.Text = "Забираю модель у участников…";
                        await Task.Delay(2000);
                    }
                    if (wanted() && Live()) note.Text = "Модель не пришла: участники с ней не в сети.";
                }
                catch (Exception ex) { if (wanted()) note.Text = $"Не удалось показать модель: {WorkspaceErrorText.ForCoreMessage(ex.Message)}"; }
                finally { drawing = false; }
            }
            void Turn(float dy, float dp, bool absolute = false)
            {
                if (absolute) { yaw = dy; pitch = dp; } else { yaw += dy; pitch = Math.Clamp(pitch + dp, -89, 89); }
                _ = Draw();
            }
            turns.Children.Add(PlainButton("⟲ 45°", false, () => Turn(-45, 0)));
            turns.Children.Add(PlainButton("⟳ 45°", false, () => Turn(45, 0)));
            turns.Children.Add(PlainButton("Изометрия", false, () => Turn(35, 25, true)));
            turns.Children.Add(PlainButton("Спереди", false, () => Turn(0, 0, true)));
            turns.Children.Add(PlainButton("Сверху", false, () => Turn(0, 89, true)));
            global::Windows.Foundation.Point? from = null;
            frame.PointerPressed += (_, e) => { from = e.GetCurrentPoint(frame).Position; frame.CapturePointer(e.Pointer); };
            frame.PointerReleased += (_, e) =>
            {
                if (from is not { } start) return;
                global::Windows.Foundation.Point end = e.GetCurrentPoint(frame).Position;
                from = null;
                frame.ReleasePointerCapture(e.Pointer);
                if (Math.Abs(end.X - start.X) + Math.Abs(end.Y - start.Y) > 4) Turn((float)(end.X - start.X) * 0.5f, (float)(end.Y - start.Y) * 0.5f);
            };
            await Draw();
        }

        // A file Windows can draw a picture of through its program (NX, SolidWorks and the like put a
        // thumbnail handler in the system): shown when there is one, nothing when only an icon comes.
        async Task ShowThumbnail(StackPanel into, Func<bool> wanted, string path)
        {
            try
            {
                WorkspacePreview? here = (await Task.Run(() => core.WorkspaceFiles(new("export", id, repo, Path: path)))).Export;
                if (here?.File is not { } file || !wanted() || !Live()) return;
                global::Windows.Storage.StorageFile item = await global::Windows.Storage.StorageFile.GetFileFromPathAsync(file);
                using global::Windows.Storage.FileProperties.StorageItemThumbnail thumbnail = await item.GetThumbnailAsync(global::Windows.Storage.FileProperties.ThumbnailMode.SingleItem, 400, global::Windows.Storage.FileProperties.ThumbnailOptions.ResizeThumbnail);
                if (thumbnail is null || thumbnail.Type != global::Windows.Storage.FileProperties.ThumbnailType.Image || !wanted() || !Live()) return;
                Microsoft.UI.Xaml.Media.Imaging.BitmapImage bitmap = new();
                await bitmap.SetSourceAsync(thumbnail);
                into.Children.Add(SectionLabel("Эскиз"));
                into.Children.Add(new Border
                {
                    BorderThickness = new Thickness(1),
                    BorderBrush = MainWindowPresentation.ResolveThemeBrush("OmniPopupStrokeBrush", new Microsoft.UI.Xaml.Media.SolidColorBrush(Microsoft.UI.Colors.Gray)),
                    HorizontalAlignment = HorizontalAlignment.Left,
                    Child = new Image { Source = bitmap, Stretch = Microsoft.UI.Xaml.Media.Stretch.Uniform, MaxHeight = 320 },
                });
                into.Children.Add(Caption("Эскиз, который Windows получает от программы этого файла."));
            }
            catch (Exception) { /* No thumbnail handler or the file is busy: the page simply has no picture. */ }
        }

        // One picture: the content from here or, asked for, from the members within a minute. It does
        // not scroll the page to itself — the person may be reading something else by then. `wanted`:
        // whether this picture is still the one asked for (another file, variant or page drops it).
        async Task ShowPane(Border cell, Func<bool> wanted, string path, string? oid, string label, double height)
        {
            cell.Child = Caption($"{label}: загружаю…");
            try
            {
                bool waited = false;
                for (int attempt = 0; attempt < 30 && wanted() && Live(); attempt++)
                {
                    WorkspacePreview? shown = (await Task.Run(() => core.WorkspaceFiles(new("preview", id, repo, Path: path, Oid: oid)))).Preview;
                    if (!wanted() || !Live()) return;
                    if (shown?.File is { } file)
                    {
                        StackPanel pane = new() { Spacing = 4 };
                        pane.Children.Add(Caption(label));
                        FrameworkElement picture = await RenderPreviewAsync(file, path, height);
                        if (!wanted() || !Live()) return;
                        pane.Children.Add(picture);
                        cell.Child = pane;
                        return;
                    }
                    if (shown?.Pending != true) break;
                    waited = true;
                    await Task.Delay(2000);
                }
                if (wanted() && Live())
                {
                    StackPanel failed = new() { Spacing = 8 };
                    failed.Children.Add(Body(waited ? $"{label}: содержимое не пришло за минуту." : $"{label}: содержимое не удалось получить."));
                    failed.Children.Add(PlainButton("Повторить", false, () => _ = ShowPane(cell, wanted, path, oid, label, height)));
                    cell.Child = failed;
                }
            }
            catch (Exception ex) { if (wanted()) cell.Child = Body($"{label}: {ex.Message}"); }
        }

        // «Беру в работу»: the folder must hold the latest version first (the core checks); the
        // others then see the file as read-only. Taken: «Продлить» a day, «Отдать». Someone took
        // it first: our change goes aside. An admin can give back someone else's file.
        void DrawWork(StackPanel work, StackPanel buttons, WorkspaceFile file)
        {
            work.Children.Clear();
            buttons.Children.Clear();
            if (view is null || inspected is not null || !file.InVersion) return;
            if (file.Lock is { Mine: true } mine)
            {
                work.Children.Add(StateLine("В работе у вас", "Accent", $"до {MainWindowPresentation.FormatMessageTime(mine.UntilUnixMs)}"));
                buttons.Children.Add(PlainButton("Продлить на сутки", false, () => _ = Act(new("take", id, repo, Paths: [file.Path]))));
                buttons.Children.Add(PlainButton("Отдать", false, () => _ = Act(new("give", id, repo, Paths: [file.Path]))));
            }
            else if (file.Lock is { } theirs)
            {
                work.Children.Add(StateLine($"В работе: {theirs.Name}", "Caution", $"В работе у {theirs.Name} до {MainWindowPresentation.FormatMessageTime(theirs.UntilUnixMs)} — у вас только для чтения."
                    + (file.TakenAfter ? $" Вы тоже брали его, но {theirs.Name} — раньше." : "")));
                if (file.State == "changed")
                {
                    work.Children.Add(Body($"Ваша правка с ним разойдётся. Отложите её рядом — «{file.Path}» вернётся к версии папки, а ваша копия сохранится с вашим именем при следующем «Сохранить версию»."));
                    buttons.Children.Add(PlainButton("Отложить мою правку рядом", true, async () =>
                    {
                        if (await Act(new("aside", id, repo, Path: file.Path)))
                            Say($"Правка отложена рядом. «{file.Path}» вернётся к версии папки в течение нескольких секунд.");
                    }));
                }
                if (view.IAmAdmin)
                {
                    if (askingUnlock == file.Path) { AskUnlock(work, buttons, file, theirs); return; }
                    buttons.Children.Add(PlainButton("Снять замок…", false, () => { askingUnlock = file.Path; AskUnlock(work, buttons, file, theirs); }));
                }
            }
            else if (view.WorkPath is not null)
            {
                if (view.Policy.EditOnlyTaken) work.Children.Add(Caption("Править — только взяв в работу."));
                buttons.Children.Add(PlainButton("Беру в работу", true, () => _ = Act(new("take", id, repo, Paths: [file.Path]))));
            }
        }
        // Asked in place: the page holds the question, no dialog over it.
        void AskUnlock(StackPanel work, StackPanel buttons, WorkspaceFile file, WorkspaceLock theirs)
        {
            work.Children.Clear();
            buttons.Children.Clear();
            work.Children.Add(Body($"Снять замок {theirs.Name} с «{file.Path}»? Если {theirs.Name} не сохранил изменения, они останутся у него в папке. Файл станет свободен для всех."));
            StackPanel answer = new() { Orientation = Orientation.Horizontal, Spacing = 8 };
            answer.Children.Add(PlainButton("Снять замок", true, async () =>
            {
                askingUnlock = null;
                if (await Act(new("unlock", id, repo, Path: file.Path, AccountId: theirs.AccountId)))
                    Say($"Замок {theirs.Name} снят: «{file.Path}» свободен.");
            }));
            answer.Children.Add(PlainButton("Отмена", false, () => { askingUnlock = null; DrawWork(work, buttons, file); }));
            work.Children.Add(answer);
        }

        // «Выбрать итоговую»: the file and each copy, any of them opened first; the chosen one becomes
        // a new version (accepted as any save, undone from «Истории»): it stays as the file, the copies go.
        void DrawConflict(WorkspaceConflict conflict)
        {
            details.Children.Add(new TextBlock { Text = conflict.Path, Style = (Style)Application.Current.Resources["CardTitleTextStyle"], TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true });
            details.Children.Add(StateLine("Расхождение", "Caution", $"Вариантов: {conflict.Copies.Count + 1}. Выбранный останется под именем файла, остальные уйдут у всех; вернуть прежнее можно из «Истории»."));
            List<(string Path, string Label)> options = [(conflict.Path, "Файл как сейчас")];
            options.AddRange(conflict.Copies.Select(c => (c.Path, $"Правка {c.Label}")));
            // Nothing is chosen before the variants are looked at: «Сделать итоговой» waits for a pick.
            string? chosen = options.Any(o => o.Path == chosenVariant) ? chosenVariant : null;
            Button make = PlainButton("Сделать итоговой", true, null);
            make.IsEnabled = chosen is not null;
            Border picture = new();
            int ticket = previewTicket;
            // Each choice asks for its own picture; a slower answer for the one before is dropped.
            int asked = 0;
            void Picture(string path, string label)
            {
                int mine = ++asked;
                _ = ShowPane(picture, () => mine == asked && ticket == previewTicket, path, null, label, 360);
            }
            StackPanel list = new() { Spacing = 6 };
            foreach ((string path, string label) in options)
            {
                Grid line = new() { ColumnSpacing = 12 };
                line.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
                line.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
                RadioButton pick = new() { GroupName = "conflict:" + conflict.Path, IsChecked = path == chosen, Content = $"{label} — {Path.GetFileName(path)}" };
                pick.Checked += (_, _) =>
                {
                    chosen = path; chosenVariant = path;
                    make.IsEnabled = true;
                    if (Previewable(conflict.Path)) Picture(path, label);
                };
                line.Children.Add(pick);
                Button open = PlainButton("Открыть", false, () => _ = OpenFile(path, null, ""));
                Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(open, $"Открыть: {label}");
                Grid.SetColumn(open, 1);
                line.Children.Add(open);
                list.Children.Add(line);
            }
            details.Children.Add(list);
            make.Click += (_, _) =>
            {
                if (chosen is not { } keep) return;
                _ = Choose(conflict.Path, keep, keep == conflict.Path ? "Итоговая — файл как есть." : $"Итоговая — {options.First(o => o.Path == keep).Label.ToLowerInvariant()}.");
            };
            details.Children.Add(make);
            if (Previewable(conflict.Path))
            {
                details.Children.Add(picture);
                if (chosen is not null) Picture(chosen, options.First(o => o.Path == chosen).Label);
            }
        }
        // The choice is a version like any save: an admin's or a custodian's is accepted at once, a
        // member's waits — told by the accepted version still holding the copies.
        async Task Choose(string path, string keep, string done)
        {
            if (!await Act(new("choose", id, repo, Path: path, Chosen: keep))) return;
            bool waiting = view?.Conflicts?.Any(c => c.Path == path) == true;
            chosenVariant = null;
            if (!waiting) selectedConflict = null;
            Say(waiting
                ? done + " Это новая версия: она ждёт принятия админом или хранителем, до тех пор копии остаются."
                : done + " Версия принята — папки участников обновятся в течение нескольких секунд.",
                waiting ? StatusKind.Warning : StatusKind.Done);
            DrawDetails();
        }

        // «На этом устройстве»: the project's top folders; an unchecked one is not taken into the
        // working folder here (and leaves the disk unless something in it is changed).
        void DrawLeftOut(List<WorkspaceFolder> folders)
        {
            details.Children.Add(SectionLabel("На этом устройстве"));
            details.Children.Add(Caption("Снятая папка не забирается в рабочую папку и уходит с диска, если в ней нет несохранённых правок. У проекта и у других участников она остаётся."));
            List<(CheckBox Box, string Path)> boxes = [];
            StackPanel list = new() { Spacing = 0 };
            foreach (WorkspaceFolder folder in folders)
            {
                CheckBox box = new() { Content = $"{folder.Path} · {Megabytes(folder.Bytes)}", IsChecked = view?.LeftOut?.Contains(folder.Path) != true };
                boxes.Add((box, folder.Path));
                list.Children.Add(box);
            }
            details.Children.Add(list);
            details.Children.Add(PlainButton("Применить", false, async () =>
            {
                List<string> leftOut = boxes.Where(b => b.Box.IsChecked != true).Select(b => b.Path).ToList();
                if (await Act(new("leave_out", id, repo, Folders: leftOut)))
                    Say(leftOut.Count == 0 ? "Все папки проекта забираются сюда." : $"Не забирается сюда: {string.Join(", ", leftOut)}. Папка обновится в течение нескольких секунд.");
            }));
        }

        // «Открыть»: the file in its program — the working folder's own while it holds this version, a
        // read-only copy of an older one; content not here is fetched first (up to a minute).
        async Task OpenFile(string path, string? oid, string which)
        {
            bool waited = false;
            try
            {
                for (int attempt = 0; attempt < 30 && Live(); attempt++)
                {
                    WorkspacePreview? ready = (await Task.Run(() => core.WorkspaceFiles(new("export", id, repo, Path: path, Oid: oid)))).Export;
                    // The page was left meanwhile: no program opens behind the person's back.
                    if (!Live()) return;
                    if (ready?.File is { } file)
                    {
                        System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo { FileName = file, UseShellExecute = true });
                        Say(oid is null ? $"Открыт {path}." : $"Открыта {which}{path} — копия только для чтения.");
                        return;
                    }
                    if (ready?.Pending != true) break;
                    Say($"Забираю {which}{path} у участников…", StatusKind.Working);
                    waited = true;
                    await Task.Delay(2000);
                }
                Say(waited ? $"Содержимое {which}{path} не пришло за минуту. Попробуйте ещё раз позже." : $"Содержимое {which}{path} не удалось получить.", StatusKind.Warning);
            }
            catch (Exception ex) { Say(ex.Message, StatusKind.Failed); }
        }

        async Task Download(WorkspaceFile file)
        {
            if (PickSaveFile is null) return;
            string? target = PickSaveFile(Path.GetFileName(file.Path));
            if (target is null) return;
            if (await Act(new("download", id, repo, Path: file.Path, Target: target, Oid: inspected)))
                Say("Загрузка запрошена. Она продолжится, когда источник будет в сети; «Обновить» покажет результат.", StatusKind.Working);
        }

        chooseTab = chosen =>
        {
            if (chosen == shownTab) return;
            shownTab = chosen;
            if (inspected is not null) { SwitchVersion(null); inspectedLabel = null; pickFirst = true; }
            if (chosen == TabFiles || chosen == TabProposals) _ = ReloadSafe();
            else if (chosen == TabHistory) _ = FillHistoryAsync(repo, historyHost, pendingRevision, Live, ShowVersion, historyChosen);
            else _ = FillSettingsAsync(repo, settingsBody, Live);
            ShowTab();
        };
        ShowTab();
        if (shownTab == TabHistory) _ = FillHistoryAsync(repo, historyHost, pendingRevision, Live, ShowVersion, historyChosen);
        else if (shownTab == TabSettings) _ = FillSettingsAsync(repo, settingsBody, Live);

        try
        {
            await Reload();
            Arrange();
        }
        catch (Exception ex)
        {
            Say($"Не удалось открыть файлы проекта: {ex.Message}", StatusKind.Failed);
        }
    }

    // A file of the list: its name, its folder and size under it (where its content is — only when it
    // is not here), what this computer or the proposal did to it (a chip).
    private FrameworkElement FileRow(WorkspaceFile file, bool changedInProposal = false, bool ownRejected = false)
    {
        Grid row = new() { ColumnSpacing = 8, Padding = new Thickness(0, 6, 0, 6) };
        row.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        StackPanel text = new() { Spacing = 2, VerticalAlignment = VerticalAlignment.Center };
        TextBlock name = new() { Text = Path.GetFileName(file.Path), TextTrimming = TextTrimming.CharacterEllipsis };
        ToolTipService.SetToolTip(name, file.Path);
        text.Children.Add(name);
        string folder = Path.GetDirectoryName(file.Path)?.Replace('\\', '/') ?? "";
        List<string> facts = [];
        if (folder.Length > 0) facts.Add(folder);
        facts.Add(Megabytes(file.Bytes));
        if (file.LeftOut) facts.Add("не забирается сюда");
        else if (!file.AvailableHere) facts.Add("у участников");
        text.Children.Add(Caption(string.Join(" · ", facts)));
        row.Children.Add(text);
        StackPanel chips = new() { Orientation = Orientation.Horizontal, Spacing = 6, VerticalAlignment = VerticalAlignment.Center };
        if (file.Lock is { } held)
            chips.Children.Add(Chip(held.Mine ? "В работе у вас" : $"В работе: {held.Name}", held.Mine ? "Accent" : "Caution"));
        if (changedInProposal) chips.Children.Add(Chip("Изменён", "Caution"));
        else if (file.State switch { "changed" => Chip("Есть изменения", "Caution"), "saved" => ownRejected ? Chip("Не принята", "Critical") : Chip("Ждёт принятия", "Accent"), _ => null } is { } chip)
            chips.Children.Add(chip);
        Grid.SetColumn(chips, 1);
        row.Children.Add(chips);
        return row;
    }

    // What a screen reader says for a row: the path, then the states the chips show.
    private static string FileRowName(WorkspaceFile file)
    {
        List<string> parts = [file.Path];
        if (file.Lock is { } held) parts.Add(held.Mine ? "в работе у вас" : $"в работе у {held.Name}");
        if (file.State == "changed") parts.Add("есть изменения");
        else if (file.State == "saved") parts.Add("ждёт принятия");
        parts.Add(file.LeftOut ? "не забирается сюда" : file.AvailableHere ? "на этом устройстве" : "у участников");
        return string.Join(", ", parts);
    }
}
