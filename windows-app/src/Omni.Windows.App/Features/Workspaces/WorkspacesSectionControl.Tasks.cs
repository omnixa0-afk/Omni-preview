using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Omni.Windows.App.Presentation;

namespace Omni.Windows.App.Features.Workspaces;

/// «Задачи» of a workspace: a board of four columns over the tasks project's files
/// (docs/decisions/omni-workspace-git.md, раздел 3). A task opens in a dialog with its fields,
/// description, comments and the versions that named it; every change is a version of the tasks
/// project, accepted at once.
public sealed partial class WorkspacesSectionControl
{
    // The tasks project (the core's `TASKS_REPO`): its page is this board, not a project page.
    private const string TasksRepo = "omni-tasks";

    private static readonly (string Status, string Column, string Next, string NextLabel)[] TaskColumns =
    [
        ("new", "Новые", "doing", "В работу"),
        ("doing", "В работе", "review", "На проверку"),
        ("review", "На проверке", "done", "Готово"),
        ("done", "Готово", "", ""),
    ];

    // A column of the board never gets narrower: four of them fit a 1280 window beside the list column.
    private const double BoardColumnMin = 190;

    // Board or list as the person chose, kept while the app runs. Not chosen: the board where its four
    // columns fit, the list where they do not (half a board hides what is in the rest).
    private bool? tasksViewChosen;

    private static string TaskStatusName(string status) => TaskColumns.FirstOrDefault(c => c.Status == status).Column ?? "Новые";

    /// The board, in the section instead of the projects («Задачи» of the switcher).
    internal async Task ShowTasksAsync(string? openTask = null)
    {
        if (bridge is null || groupId is null) return;
        RustBridge core = bridge; string id = groupId;
        if (!ProjectOpen) cardsOffset = WorkspaceCardsScroll.VerticalOffset;
        int page = ++projectTicket;
        projectRepo = TasksRepo;
        projectScreenChanged = null;
        ShowSwitcher(Place.Tasks);
        bool Live() => page == projectTicket;
        string onlyWho = "", onlyLabel = "";
        bool busy = false, dialogOpen = false, dragging = false;
        // A card is carried by the board itself, not by the system's drag and drop (whose start takes a
        // third of a second): its copy follows the pointer, the column under it shows where it lands.
        List<DropColumn> drops = new();
        WorkspaceTasks? board = null;
        // What the board drew last: a read that brings nothing new does not rebuild the cards (a card
        // pressed for a drag would be replaced under the mouse).
        string drawn = string.Empty;

        Grid frame = new() { Padding = UnscrolledPadding };
        frame.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        frame.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });

        // The filters and the board's buttons are the place's: on the places' strip, at the board's right
        // edge — the board starts right under it.
        ComboBox who = new() { MinWidth = 180 };
        ComboBox labels = new() { MinWidth = 150 };
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(who, "Исполнитель");
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(labels, "Метка");
        Button add = PlainButton("Новая задача", true, null);
        Button refresh = PlainButton("Обновить", false, null);
        // Board or list: the same tasks; the list reads on any width and takes no sideways scroll.
        Action<string> markView = _ => { };
        Action redraw = () => { };
        (FrameworkElement viewSwitch, markView, _) = Segments([("board", "Доска"), ("list", "Список")], tag =>
        {
            tasksViewChosen = tag == "list";
            redraw();
        });
        bool shownAsList = false;
        bool AsList() => tasksViewChosen
            ?? (frame.ActualWidth > 0 && frame.ActualWidth - UnscrolledPadding.Left - UnscrolledPadding.Right < 4 * BoardColumnMin + 3 * 12);
        SetPlaceBar(add, viewSwitch, who, labels, refresh);

        void Say(string text, StatusKind kind = StatusKind.Done)
        {
            if (Live()) SetStatus(text, kind);
        }

        // Four columns side by side; a narrow window scrolls them sideways rather than squeezing a column
        // under the width a card's title reads in.
        Grid columns = new() { ColumnSpacing = 12, MinWidth = 4 * BoardColumnMin + 3 * 12 };
        ScrollViewer across = new() { Content = columns, HorizontalScrollBarVisibility = ScrollBarVisibility.Auto, HorizontalScrollMode = ScrollMode.Auto, VerticalScrollBarVisibility = ScrollBarVisibility.Disabled, VerticalScrollMode = ScrollMode.Disabled };
        Grid.SetRow(across, 1);
        frame.Children.Add(across);
        // A sideways scroller measures its content with no width limit, so star columns would take their
        // longest title's width and run off the edge: they get the visible width, or their minimum.
        across.SizeChanged += (_, e) => columns.Width = Math.Max(e.NewSize.Width, columns.MinWidth);
        Features.Shared.PageColumns rows = new() { Spacing = 6, ColumnGap = 24, MinColumn = 420 };
        ScrollViewer rowsScroll = new() { Content = rows, VerticalScrollBarVisibility = ScrollBarVisibility.Auto, Visibility = Visibility.Collapsed };
        Grid.SetRow(rowsScroll, 1);
        frame.Children.Add(rowsScroll);
        // Above everything: the carried card's copy.
        Canvas overlay = new() { IsHitTestVisible = false };
        Grid.SetRowSpan(overlay, 2);
        frame.Children.Add(overlay);

        WorkspaceProjectHost.Children.Clear();
        WorkspaceProjectHost.Children.Add(frame);
        WorkspaceCardsScroll.Visibility = Visibility.Collapsed;
        WorkspaceProjectHost.Visibility = Visibility.Visible;

        async Task<bool> Run(WorkspaceFilesRequest request, string done)
        {
            if (busy) return false;
            busy = true;
            try
            {
                WorkspaceTasks? after = (await Task.Run(() => core.WorkspaceFiles(request))).Tasks;
                if (!Live()) return true;
                if (after is not null) { board = after; Draw(); }
                Say(done);
                return true;
            }
            catch (Exception ex)
            {
                Say(WorkspaceErrorText.ForCoreMessage(ex.Message), StatusKind.Failed);
                return false;
            }
            finally { busy = false; }
        }

        // The same write for a dialog: what went wrong comes back to it, to be said there; null — written.
        async Task<string?> Write(WorkspaceFilesRequest request)
        {
            if (busy) return "Предыдущее изменение ещё записывается — попробуйте ещё раз.";
            busy = true;
            try
            {
                WorkspaceTasks? after = (await Task.Run(() => core.WorkspaceFiles(request))).Tasks;
                if (Live() && after is not null) { board = after; Draw(); }
                return null;
            }
            catch (Exception ex) { return WorkspaceErrorText.ForCoreMessage(ex.Message); }
            finally { busy = false; }
        }

        async Task Reload()
        {
            try
            {
                WorkspaceTasks? fresh = (await Task.Run(() => core.WorkspaceFiles(new("tasks", id, TasksRepo)))).Tasks;
                if (!Live() || fresh is null) return;
                board = fresh;
                if (BoardSignature(fresh) + "|" + onlyWho + "|" + onlyLabel != drawn) Draw();
            }
            catch (Exception ex) { Say($"Не удалось прочитать задачи: {WorkspaceErrorText.ForCoreMessage(ex.Message)}", StatusKind.Failed); }
        }

        void Draw()
        {
            columns.Children.Clear();
            columns.ColumnDefinitions.Clear();
            drops.Clear();
            if (board is null) return;
            drawn = BoardSignature(board) + "|" + onlyWho + "|" + onlyLabel;
            add.IsEnabled = board.Enabled;
            who.Visibility = labels.Visibility = viewSwitch.Visibility = board.Enabled && board.Tasks.Count > 0 ? Visibility.Visible : Visibility.Collapsed;
            bool asList = board.Enabled && AsList();
            shownAsList = asList;
            markView(asList ? "list" : "board");
            across.Visibility = asList ? Visibility.Collapsed : Visibility.Visible;
            rowsScroll.Visibility = asList ? Visibility.Visible : Visibility.Collapsed;
            if (!board.Enabled)
            {
                StackPanel off = new() { Spacing = 10, MaxWidth = 560, HorizontalAlignment = HorizontalAlignment.Left };
                off.Children.Add(Body("Задачи в этом workspace ещё не включены. Задача — это файл в общем проекте задач: у неё номер, исполнитель, метки, ссылки на файлы проекта и обсуждение. Каждая правка сразу видна всем."));
                if (board.CanEnable)
                    off.Children.Add(PlainButton("Включить задачи", true, () => _ = Run(new("tasks_enable", id, TasksRepo), "Задачи включены.")));
                else
                    off.Children.Add(Caption("Включает админ workspace."));
                columns.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
                columns.Children.Add(off);
                columns.MinWidth = 0;
                return;
            }
            columns.MinWidth = 4 * BoardColumnMin + 3 * 12;
            DrawFilters();
            IEnumerable<WorkspaceTask> shown = board.Tasks;
            if (onlyWho == "-") shown = shown.Where(t => t.Assignee is null);
            else if (onlyWho.Length > 0) shown = shown.Where(t => t.Assignee == onlyWho);
            if (onlyLabel.Length > 0) shown = shown.Where(t => t.Labels.Contains(onlyLabel));
            List<WorkspaceTask> tasks = shown.ToList();
            if (asList)
            {
                DrawRows(tasks);
                return;
            }
            for (int i = 0; i < TaskColumns.Length; i++)
            {
                (string state, string name, string next, string nextLabel) = TaskColumns[i];
                columns.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star), MinWidth = BoardColumnMin });
                List<WorkspaceTask> here = tasks.Where(t => t.Status == state).OrderBy(t => t.Place).ToList();
                // A card dropped here takes this column's status and the place the line shows.
                Grid column = new() { RowSpacing = 2, Background = new Microsoft.UI.Xaml.Media.SolidColorBrush(Microsoft.UI.Colors.Transparent), CornerRadius = new CornerRadius(8) };
                StackPanel cards = new() { Spacing = 8, Padding = new Thickness(0, 6, 8, 8) };
                // The line where the card lands: its negative margins take back the room it is given, so
                // the cards around it do not move under the pointer.
                Border line = new()
                {
                    Height = 4,
                    Margin = new Thickness(0, -6, 0, -6),
                    CornerRadius = new CornerRadius(2),
                    IsHitTestVisible = false,
                    Background = MainWindowPresentation.ResolveThemeBrush("OmniAccentBrush", new Microsoft.UI.Xaml.Media.SolidColorBrush(Microsoft.UI.Colors.SteelBlue)),
                };
                void Unmark()
                {
                    cards.Children.Remove(line);
                    column.Background = new Microsoft.UI.Xaml.Media.SolidColorBrush(Microsoft.UI.Colors.Transparent);
                }
                // The card the carried one would go above (null: the column's end), and whether that is
                // where it already is.
                (FrameworkElement? Above, bool Same) Landing(Func<UIElement, global::Windows.Foundation.Point> at, string taskId)
                {
                    List<FrameworkElement> placed = cards.Children.OfType<FrameworkElement>().Where(c => c.Tag is string).ToList();
                    List<FrameworkElement> others = placed.Where(c => (string)c.Tag != taskId).ToList();
                    FrameworkElement? above = others.FirstOrDefault(c => at(c).Y < c.ActualHeight / 2);
                    int own = placed.FindIndex(c => (string)c.Tag == taskId);
                    return (above, own >= 0 && own == (above is null ? others.Count : others.IndexOf(above)));
                }
                void Hover(Func<UIElement, global::Windows.Foundation.Point> at, string taskId)
                {
                    (FrameworkElement? above, bool same) = Landing(at, taskId);
                    column.Background = same
                        ? new Microsoft.UI.Xaml.Media.SolidColorBrush(Microsoft.UI.Colors.Transparent)
                        : MainWindowPresentation.ResolveThemeBrush("OmniAccentSoftBrush", new Microsoft.UI.Xaml.Media.SolidColorBrush(Microsoft.UI.ColorHelper.FromArgb(24, 128, 128, 128)));
                    int now = cards.Children.IndexOf(line);
                    bool shown = !same && now >= 0 && (above is null ? now == cards.Children.Count - 1 : now == cards.Children.IndexOf(above) - 1);
                    if (!shown)
                    {
                        cards.Children.Remove(line);
                        if (!same) cards.Children.Insert(above is null ? cards.Children.Count : cards.Children.IndexOf(above), line);
                    }
                }
                async Task Drop(Func<UIElement, global::Windows.Foundation.Point> at, string taskId)
                {
                    (FrameworkElement? above, bool same) = Landing(at, taskId);
                    Unmark();
                    if (same || board?.Tasks.FirstOrDefault(t => t.Id == taskId) is not { } moved) return;
                    bool elsewhere = moved.Status != state;
                    // The card stands where it was dropped at once; the saved board comes after (and puts
                    // it back if the change is refused).
                    WorkspaceTasks saved = board!;
                    List<WorkspaceTask> rest = saved.Tasks.Where(t => t.Status == state && t.Id != taskId).OrderBy(t => t.Place).ToList();
                    int slot = above is null ? rest.Count : rest.FindIndex(t => t.Id == (string)above.Tag);
                    double place = (slot > 0 ? rest[slot - 1].Place : (double?)null, slot < rest.Count ? rest[slot].Place : (double?)null) switch
                    {
                        ({ } up, { } down) => (up + down) / 2,
                        ({ } up, null) => up + 1000,
                        (null, { } down) => down - 1000,
                        _ => 0,
                    };
                    board = saved with { Tasks = saved.Tasks.Select(t => t.Id == taskId ? t with { Status = state, Place = place } : t).ToList() };
                    Draw();
                    board = saved;
                    if (!await Run(new("task_change", id, TasksRepo, Task: taskId, Status: elsewhere ? state : null, Before: (string?)above?.Tag ?? ""),
                            elsewhere ? $"#{moved.Number} — «{name}»." : $"#{moved.Number} переставлена.") && Live())
                        Draw();
                }
                drops.Add(new DropColumn(column, Hover, Unmark, Drop));
                column.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
                column.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
                column.Children.Add(SectionLabel($"{name} · {here.Count}"));
                foreach (WorkspaceTask task in here) cards.Children.Add(TaskCard(task, next, nextLabel));
                if (here.Count == 0) cards.Children.Add(Caption(state == "new" ? "Новых задач нет." : "Пусто."));
                ScrollViewer down = new() { Content = cards, VerticalScrollBarVisibility = ScrollBarVisibility.Auto };
                Grid.SetRow(down, 1);
                column.Children.Add(down);
                Grid.SetColumn(column, i);
                columns.Children.Add(column);
            }
        }

        // The list: the board's columns as groups, the same step to the next one; moved by that step or in
        // the task's dialog, not carried.
        void DrawRows(List<WorkspaceTask> tasks)
        {
            rows.Children.Clear();
            foreach ((string state, string name, string next, string nextLabel) in TaskColumns)
            {
                List<WorkspaceTask> here = tasks.Where(t => t.Status == state).OrderBy(t => t.Place).ToList();
                if (here.Count == 0) continue;
                rows.Children.Add(SectionTitle($"{name} · {here.Count}"));
                foreach (WorkspaceTask task in here) rows.Children.Add(TaskRow(task, next, nextLabel));
            }
            if (rows.Children.Count == 0) rows.Children.Add(Caption("Таких задач нет."));
        }

        FrameworkElement TaskRow(WorkspaceTask task, string next, string nextLabel)
        {
            Grid row = new() { ColumnSpacing = 12 };
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            StackPanel text = new() { Spacing = 2, VerticalAlignment = VerticalAlignment.Center };
            text.Children.Add(new TextBlock { Text = $"#{task.Number} {task.Title}", FontWeight = Microsoft.UI.Text.FontWeights.SemiBold, TextTrimming = TextTrimming.CharacterEllipsis });
            string meta = (task.AssigneeName ?? "без исполнителя")
                + (task.Comments.Count > 0 ? $" · комментариев: {task.Comments.Count}" : "")
                + (task.Labels.Count > 0 ? " · " + string.Join(", ", task.Labels) : "")
                + (task.Conflict ? " · две правки описания" : "");
            text.Children.Add(Caption(meta));
            row.Children.Add(text);
            if (next.Length > 0)
            {
                Button step = PlainButton($"{nextLabel} →", false, () => _ = Run(new("task_change", id, TasksRepo, Task: task.Id, Status: next), $"#{task.Number} — «{TaskStatusName(next)}»."));
                step.VerticalAlignment = VerticalAlignment.Center;
                Grid.SetColumn(step, 1);
                row.Children.Add(step);
            }
            FrameworkElement card = Card(row);
            card.Tapped += (_, e) =>
            {
                for (DependencyObject? at = e.OriginalSource as DependencyObject; at is not null && at != card; at = Microsoft.UI.Xaml.Media.VisualTreeHelper.GetParent(at))
                    if (at is Microsoft.UI.Xaml.Controls.Primitives.ButtonBase) return;
                _ = OpenTask(task.Id);
            };
            return card;
        }

        // The filters keep what is chosen through a redraw; their lists come from the board and are
        // filled again only when what they list changed — a list emptied while it closes on a pick
        // takes the window down («element not found» from XAML).
        bool filling = false;
        string whoListed = "", labelsListed = "";
        void DrawFilters()
        {
            if (board is null) return;
            filling = true;
            List<(string Content, string Tag)> people = [("Все исполнители", ""), ("Без исполнителя", "-")];
            people.AddRange(board.Members.OrderByDescending(m => m.IsMe).ThenBy(m => m.Name).Select(m => (m.IsMe ? $"{m.Name} (вы)" : m.Name, m.AccountId)));
            Fill(who, people, ref whoListed);
            who.SelectedItem = who.Items.OfType<ComboBoxItem>().FirstOrDefault(i => (string)i.Tag == onlyWho) ?? who.Items[0];
            List<(string Content, string Tag)> marks = [("Все метки", "")];
            marks.AddRange(board.Tasks.SelectMany(t => t.Labels).Distinct().OrderBy(l => l).Select(l => (l, l)));
            Fill(labels, marks, ref labelsListed);
            labels.SelectedItem = labels.Items.OfType<ComboBoxItem>().FirstOrDefault(i => (string)i.Tag == onlyLabel) ?? labels.Items[0];
            filling = false;
        }
        static void Fill(ComboBox box, List<(string Content, string Tag)> items, ref string listed)
        {
            string now = string.Join("\n", items.Select(i => i.Content + "\t" + i.Tag));
            if (now == listed) return;
            listed = now;
            box.Items.Clear();
            foreach ((string content, string tag) in items) box.Items.Add(new ComboBoxItem { Content = content, Tag = tag });
        }
        // The board is drawn again after the pick is done, not inside it.
        who.SelectionChanged += (_, _) => { if (!filling && who.SelectedItem is ComboBoxItem { Tag: string tag }) { onlyWho = tag; DispatcherQueue.TryEnqueue(Draw); } };
        labels.SelectionChanged += (_, _) => { if (!filling && labels.SelectedItem is ComboBoxItem { Tag: string tag }) { onlyLabel = tag; DispatcherQueue.TryEnqueue(Draw); } };

        // A task on the board: #N and the title, who has it, its labels; a step to the next column.
        FrameworkElement TaskCard(WorkspaceTask task, string next, string nextLabel)
        {
            StackPanel body = CardBody(task, next, nextLabel);
            // Carried once the pressed pointer moves a few pixels; the board is not read again meanwhile
            // (it would replace the card under the mouse).
            FrameworkElement card = Card(body);
            card.Tag = task.Id;
            CardCarry(card, task, next, nextLabel);
            return card;
        }

        // What a card shows; the carried copy shows the same.
        StackPanel CardBody(WorkspaceTask task, string next, string nextLabel)
        {
            StackPanel body = new() { Spacing = 6 };
            HyperlinkButton open = new() { Padding = new Thickness(0), HorizontalAlignment = HorizontalAlignment.Left };
            open.Content = new TextBlock { Text = $"#{task.Number} {task.Title}", TextWrapping = TextWrapping.Wrap, FontWeight = Microsoft.UI.Text.FontWeights.SemiBold };
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(open, $"#{task.Number} {task.Title}");
            open.Click += (_, _) => _ = OpenTask(task.Id);
            body.Children.Add(open);
            string meta = (task.AssigneeName is { } name ? name : "без исполнителя")
                + (task.Comments.Count > 0 ? $" · комментариев: {task.Comments.Count}" : "");
            body.Children.Add(Caption(meta));
            if (task.Labels.Count > 0 || task.Conflict)
            {
                StackPanel chips = new() { Orientation = Orientation.Horizontal, Spacing = 6 };
                if (task.Conflict) chips.Children.Add(Chip("Две правки описания", "Caution"));
                foreach (string label in task.Labels.Take(3)) chips.Children.Add(Chip(label, "Neutral"));
                body.Children.Add(chips);
            }
            if (next.Length > 0)
            {
                Button step = PlainButton($"{nextLabel} →", false, () => _ = Run(new("task_change", id, TasksRepo, Task: task.Id, Status: next), $"#{task.Number} — «{TaskStatusName(next)}»."));
                step.HorizontalAlignment = HorizontalAlignment.Left;
                body.Children.Add(step);
            }
            return body;
        }

        // A card follows the pointer to another column or place: its copy over the board, itself dimmed.
        void CardCarry(FrameworkElement card, WorkspaceTask task, string next, string nextLabel)
        {
            global::Windows.Foundation.Point? pressedAt = null;
            FrameworkElement? copy = null;
            long carriedUntil = 0;
            DropColumn? Under(Microsoft.UI.Xaml.Input.PointerRoutedEventArgs e) => drops.FirstOrDefault(d =>
            {
                global::Windows.Foundation.Point at = e.GetCurrentPoint(d.Column).Position;
                return at.X >= 0 && at.Y >= 0 && at.X < d.Column.ActualWidth && at.Y < d.Column.ActualHeight;
            });
            void Follow(Microsoft.UI.Xaml.Input.PointerRoutedEventArgs e)
            {
                if (copy is null || pressedAt is not { } grab) return;
                global::Windows.Foundation.Point at = e.GetCurrentPoint(overlay).Position;
                Canvas.SetLeft(copy, at.X - grab.X);
                Canvas.SetTop(copy, at.Y - grab.Y);
                DropColumn? under = Under(e);
                foreach (DropColumn other in drops.Where(d => d != under)) other.Unmark();
                under?.Hover(x => e.GetCurrentPoint(x).Position, task.Id);
            }
            void PutDown()
            {
                if (copy is null) return;
                overlay.Children.Remove(copy);
                copy = null;
                pressedAt = null;
                dragging = false;
                card.Opacity = 1;
                carriedUntil = Environment.TickCount64;
                foreach (DropColumn d in drops) d.Unmark();
            }
            card.PointerPressed += (_, e) =>
            {
                Microsoft.UI.Input.PointerPoint point = e.GetCurrentPoint(card);
                pressedAt = point.Properties.IsLeftButtonPressed && !busy ? point.Position : null;
            };
            card.PointerMoved += (_, e) =>
            {
                if (copy is not null) { Follow(e); return; }
                if (pressedAt is not { } from) return;
                Microsoft.UI.Input.PointerPoint point = e.GetCurrentPoint(card);
                if (!point.Properties.IsLeftButtonPressed) { pressedAt = null; return; }
                if (Math.Abs(point.Position.X - from.X) < 4 && Math.Abs(point.Position.Y - from.Y) < 4) return;
                if (!card.CapturePointer(e.Pointer)) { pressedAt = null; return; }
                dragging = true;
                FrameworkElement lifted = Card(CardBody(task, next, nextLabel));
                lifted.Width = card.ActualWidth;
                lifted.Opacity = 0.9;
                lifted.IsHitTestVisible = false;
                copy = lifted;
                overlay.Children.Add(copy);
                card.Opacity = 0.4;
                Follow(e);
            };
            card.PointerReleased += async (_, e) =>
            {
                if (copy is null) { pressedAt = null; return; }
                e.Handled = true;
                DropColumn? under = Under(e);
                PutDown();
                card.ReleasePointerCapture(e.Pointer);
                // Where the card lands is read before the first await, while the pointer event holds.
                if (under is not null) await under.Drop(x => e.GetCurrentPoint(x).Position, task.Id);
            };
            card.PointerCaptureLost += (_, _) => PutDown();
            // The whole card opens the task; its step button does only its step.
            card.Tapped += (_, e) =>
            {
                if (Environment.TickCount64 - carriedUntil < 300) return;
                for (DependencyObject? at = e.OriginalSource as DependencyObject; at is not null && at != card; at = Microsoft.UI.Xaml.Media.VisualTreeHelper.GetParent(at))
                    if (at is Microsoft.UI.Xaml.Controls.Primitives.ButtonBase) return;
                _ = OpenTask(task.Id);
            };
        }

        add.Click += async (_, _) =>
        {
            if (board is null || dialogOpen) return;
            StackPanel form = new() { Spacing = 10, MinWidth = 420 };
            TextBox name = new() { Header = "Название", MaxLength = 200 };
            TextBox text = new() { Header = "Описание", AcceptsReturn = true, TextWrapping = TextWrapping.Wrap, MinHeight = 100 };
            ComboBox assignee = AssigneeBox(board, null);
            TextBox tags = new() { Header = "Метки", PlaceholderText = "через запятую" };
            form.Children.Add(name); form.Children.Add(text); form.Children.Add(assignee); form.Children.Add(tags);
            ContentDialog dialog = new()
            {
                XamlRoot = XamlRoot, RequestedTheme = MainWindowPresentation.ActiveTheme,
                Title = "Новая задача", Content = form,
                PrimaryButtonText = "Создать", CloseButtonText = "Отмена", DefaultButton = ContentDialogButton.Primary,
            };
            dialogOpen = true;
            try
            {
                if (await dialog.ShowAsync() != ContentDialogResult.Primary || name.Text.Trim().Length == 0) return;
                await Run(new("task_new", id, TasksRepo, Title: name.Text, Body: text.Text, Assignee: (assignee.SelectedItem as ComboBoxItem)?.Tag as string, Labels: SplitList(tags.Text)),
                    $"Задача «{name.Text.Trim()}» создана.");
            }
            finally { dialogOpen = false; }
        };

        // A task's dialog: its number as the title and its name as the first field; the status and who
        // has it apply at once; the name, labels, files and description are saved together («Сохранить»,
        // only what changed); a comment goes at once and joins the discussion in place.
        async Task OpenTask(string taskId)
        {
            if (board?.Tasks.FirstOrDefault(t => t.Id == taskId) is not { } task || dialogOpen) return;
            StackPanel form = new() { Spacing = 10, MinWidth = 480 };
            ScrollViewer scroll = new() { Content = form, MaxHeight = 560, VerticalScrollBarVisibility = ScrollBarVisibility.Auto, Padding = new Thickness(0, 0, 12, 0) };
            InfoBar note = new() { IsOpen = false, IsClosable = true };
            form.Children.Add(note);
            if (task.Conflict)
                form.Children.Add(new InfoBar { IsOpen = true, IsClosable = false, Severity = InfoBarSeverity.Warning, Title = "Две правки описания",
                    Message = "Описание поправили двое одновременно: второе лежит рядом копией в папке задач. Перенесите нужное в описание и удалите копию." });
            TextBox name = new() { Text = task.Title, MaxLength = 200, PlaceholderText = "Название", FontSize = 18 };
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(name, "Название");
            form.Children.Add(name);
            form.Children.Add(Caption($"Создана: {task.AuthorName} · {MainWindowPresentation.FormatMessageTime(task.CreatedUnixMs)}"));
            // Work of two agents on the task stands first: it is what waits for a person's look.
            StackPanel pairHost = new() { Spacing = 8 };
            form.Children.Add(pairHost);
            ComboBox state = new() { Header = "Статус", MinWidth = 200 };
            foreach ((string s, string column, _, _) in TaskColumns) state.Items.Add(new ComboBoxItem { Content = column, Tag = s });
            state.SelectedItem = state.Items.OfType<ComboBoxItem>().First(i => (string)i.Tag == task.Status);
            ComboBox assignee = AssigneeBox(board!, task.Assignee);
            Grid pair = new() { ColumnSpacing = 12 };
            pair.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            pair.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            pair.Children.Add(state);
            Grid.SetColumn(assignee, 1);
            pair.Children.Add(assignee);
            state.HorizontalAlignment = assignee.HorizontalAlignment = HorizontalAlignment.Stretch;
            TextBox tags = new() { Header = "Метки", Text = string.Join(", ", task.Labels), PlaceholderText = "через запятую" };
            TextBox links = new() { Header = "Файлы проектов", Text = string.Join(Environment.NewLine, task.Links), PlaceholderText = "проект:путь — по одному в строке", AcceptsReturn = true, TextWrapping = TextWrapping.Wrap };
            TextBox text = new() { Header = "Описание", Text = task.Body, AcceptsReturn = true, TextWrapping = TextWrapping.Wrap, MinHeight = 100 };
            form.Children.Add(pair);
            form.Children.Add(Caption("Статус и исполнитель меняются сразу. Название, метки, файлы и описание — кнопкой «Сохранить»."));
            form.Children.Add(tags); form.Children.Add(links); form.Children.Add(text);

            // The task's comments live in its file; the conversation about the work, with the agents, is
            // its discussion in «Обсуждения».
            Grid commentsHead = new() { ColumnSpacing = 8 };
            commentsHead.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            commentsHead.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            TextBlock discussion = SectionLabel(task.Comments.Count > 0 ? $"Комментарии · {task.Comments.Count}" : "Комментарии");
            discussion.VerticalAlignment = VerticalAlignment.Center;
            commentsHead.Children.Add(discussion);
            bool toTalk = false;
            Button talkDoor = PlainButton("Обсуждение работы", false, null);
            Grid.SetColumn(talkDoor, 1);
            commentsHead.Children.Add(talkDoor);
            form.Children.Add(commentsHead);
            StackPanel comments = new() { Spacing = 8 };
            form.Children.Add(comments);
            void AddComment(string who, long at, string said)
            {
                StackPanel one = new() { Spacing = 2 };
                one.Children.Add(Caption($"{who} · {MainWindowPresentation.FormatMessageTime(at)}"));
                one.Children.Add(new TextBlock { Text = said, TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true });
                comments.Children.Add(one);
            }
            foreach (WorkspaceTaskComment comment in task.Comments) AddComment(comment.Mine ? "Вы" : comment.AuthorName, comment.AtUnixMs, comment.Text);
            Grid replyLine = new() { ColumnSpacing = 8 };
            replyLine.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            replyLine.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            TextBox reply = new() { PlaceholderText = "Комментарий", AcceptsReturn = true, TextWrapping = TextWrapping.Wrap };
            replyLine.Children.Add(reply);
            Button send = PlainButton("Отправить", false, null);
            send.VerticalAlignment = VerticalAlignment.Top;
            Grid.SetColumn(send, 1);
            replyLine.Children.Add(send);
            form.Children.Add(replyLine);
            if (task.Mentions.Count > 0)
            {
                form.Children.Add(SectionLabel("Версии, где упомянута"));
                foreach (WorkspaceTaskMention mention in task.Mentions)
                    form.Children.Add(Caption($"{mention.Repo} · «{Shorten(mention.Subject, 80)}» · {mention.AuthorName} · {MainWindowPresentation.FormatMessageTime(mention.AtUnixMs)}"));
            }
            form.Children.Add(Caption($"«закрывает #{task.Number}» в комментарии версии переносит задачу в «Готово»."));

            ContentDialog dialog = new()
            {
                XamlRoot = XamlRoot, RequestedTheme = MainWindowPresentation.ActiveTheme,
                Title = $"#{task.Number}", Content = scroll,
                PrimaryButtonText = "Сохранить", CloseButtonText = "Закрыть", DefaultButton = ContentDialogButton.Primary,
                IsPrimaryButtonEnabled = false,
            };
            // What «Сохранить» would send: only what changed.
            bool Edited() => name.Text.Trim() != task.Title || !SplitList(tags.Text).SequenceEqual(task.Labels)
                || !SplitList(links.Text).SequenceEqual(task.Links) || text.Text.Trim() != task.Body.Trim();
            void Track() => dialog.IsPrimaryButtonEnabled = Edited();
            name.TextChanged += (_, _) => Track(); tags.TextChanged += (_, _) => Track();
            links.TextChanged += (_, _) => Track(); text.TextChanged += (_, _) => Track();
            // The status and who has it: at once, the dialog stays open with what is typed in it.
            void Told(string said, bool failed = false)
            {
                note.Message = said;
                note.Severity = failed ? InfoBarSeverity.Error : InfoBarSeverity.Success;
                note.IsOpen = true;
            }
            void ShowPair(WorkspacePair? work)
            {
                pairHost.Children.Clear();
                if (work is not null)
                    pairHost.Children.Add(PairBlock(work, work.Status == "working" ? () => _ = ClosePair(work) : null));
                if (work is null || work.Status != "working")
                    pairHost.Children.Add(PairStart(work is null ? "Проработать вдвоём агентами…" : "Проработать снова…"));
            }
            async Task ClosePair(WorkspacePair work)
            {
                try
                {
                    await Task.Run(() => core.WorkspaceAgents(new("pair_close", id, Task: task.Id)));
                    ShowPair(work with { Status = "closed" });
                    Told("Проработка завершена. Черновики и замечания остались в задаче.");
                }
                catch (Exception ex) { Told(WorkspaceErrorText.ForCoreMessage(ex.Message), failed: true); }
            }
            // Two of our agents and what they work out: read when it is opened, not with every task.
            FrameworkElement PairStart(string header)
            {
                Expander open = new() { Header = header, HorizontalAlignment = HorizontalAlignment.Stretch, HorizontalContentAlignment = HorizontalAlignment.Stretch };
                StackPanel panel = new() { Spacing = 8 };
                open.Content = panel;
                bool loaded = false;
                open.Expanding += async (_, _) =>
                {
                    if (loaded) return;
                    loaded = true;
                    panel.Children.Clear();
                    panel.Children.Add(Caption("Читаю ваших агентов…"));
                    List<WorkspaceAgent> mine;
                    try { mine = (await Task.Run(() => core.WorkspaceAgents(new("view", id)))).Agents.Where(a => a.Mine && a.InGroup).OrderBy(a => a.Name).ToList(); }
                    catch (Exception ex)
                    {
                        panel.Children.Clear();
                        panel.Children.Add(Body(WorkspaceErrorText.ForCoreMessage(ex.Message)));
                        loaded = false;
                        return;
                    }
                    panel.Children.Clear();
                    panel.Children.Add(Body("Два ваших агента сначала независимо пишут черновик, потом по очереди делают замечания и правят, пока оба не согласятся или не кончатся раунды. Итог проверяете вы."));
                    if (mine.Count < 2)
                    {
                        panel.Children.Add(Caption(mine.Count == 0
                            ? "Нужны два ваших агента в этом workspace — подключите их во вкладке «Агенты»."
                            : $"Нужен второй агент: в workspace сейчас только {mine[0].Name}. Подключите ещё одного во вкладке «Агенты»."));
                        return;
                    }
                    ComboBox Pick(string title, int at)
                    {
                        ComboBox box = new() { Header = title, HorizontalAlignment = HorizontalAlignment.Stretch };
                        foreach (WorkspaceAgent agent in mine) box.Items.Add(new ComboBoxItem { Content = agent.Name, Tag = agent.AccountId });
                        box.SelectedIndex = at;
                        return box;
                    }
                    ComboBox first = Pick("Первый агент", 0), second = Pick("Второй агент", 1);
                    Grid both = new() { ColumnSpacing = 12 };
                    both.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
                    both.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
                    both.Children.Add(first);
                    Grid.SetColumn(second, 1);
                    both.Children.Add(second);
                    TextBox topic = new() { Header = "Что проработать", Text = task.Title, AcceptsReturn = true, TextWrapping = TextWrapping.Wrap };
                    NumberBox rounds = new() { Header = "Раундов не больше", Value = 4, Minimum = 1, Maximum = 10, SpinButtonPlacementMode = NumberBoxSpinButtonPlacementMode.Inline, HorizontalAlignment = HorizontalAlignment.Left };
                    Button go = PlainButton("Начать проработку", true, null);
                    panel.Children.Add(both);
                    panel.Children.Add(topic);
                    panel.Children.Add(rounds);
                    panel.Children.Add(go);
                    go.Click += async (_, _) =>
                    {
                        if (first.SelectedItem is not ComboBoxItem { Tag: string a } one || second.SelectedItem is not ComboBoxItem { Tag: string b } two) return;
                        if (a == b) { Told("Выберите двух разных агентов.", failed: true); return; }
                        int most = double.IsNaN(rounds.Value) ? 4 : (int)rounds.Value;
                        go.IsEnabled = false;
                        try
                        {
                            // Read on the UI thread: from the pool the box's Text throws.
                            string about = topic.Text.Trim();
                            await Task.Run(() => core.WorkspaceAgents(new("pair_start", id, Task: task.Id, First: a, Second: b, Topic: about, Rounds: most)));
                            ShowPair(new WorkspacePair("", about, [(string)one.Content, (string)two.Content], most, 0, "working", [], [], []));
                            Told($"{one.Content} и {two.Content} получили задание в обсуждении workspace. Ход проработки — здесь, в задаче.");
                        }
                        catch (Exception ex) { Told(WorkspaceErrorText.ForCoreMessage(ex.Message), failed: true); }
                        finally { go.IsEnabled = true; }
                    };
                };
                return open;
            }
            ShowPair(task.Pair);
            // The board waits while a dialog is open, the agents do not: their work is read here on its own,
            // and redrawn only when it moved and no start of a new one is being filled in.
            string PairSignature(WorkspacePair? work) => work is null ? "" : $"{work.Session},{work.Status},{work.Round},{work.Drafts.Count},{work.Remarks.Count(r => r.Taken is not null)},{work.Remarks.Count}";
            string pairSeen = PairSignature(task.Pair);
            DispatcherTimer pairWatch = new() { Interval = TimeSpan.FromSeconds(5) };
            pairWatch.Tick += async (_, _) =>
            {
                if (pairHost.Children.OfType<Expander>().Any(e => e.IsExpanded)) return;
                try
                {
                    WorkspacePair? now = (await Task.Run(() => core.WorkspaceFiles(new("tasks", id, TasksRepo)))).Tasks?.Tasks.FirstOrDefault(t => t.Id == task.Id)?.Pair;
                    if (PairSignature(now) == pairSeen) return;
                    pairSeen = PairSignature(now);
                    ShowPair(now);
                }
                catch (Exception) { /* the next tick reads again; the dialog keeps what it shows */ }
            };
            // One change at a time: both lists wait while one is written, and a refused one goes back to
            // what is saved — the list never shows a status the task does not have.
            async Task ChangeAtOnce(WorkspaceFilesRequest request, Action saved, Action back, string done)
            {
                state.IsEnabled = assignee.IsEnabled = false;
                string? failed = await Write(request);
                if (failed is null) { saved(); Told(done); }
                else { back(); Told(failed, failed: true); }
                state.IsEnabled = assignee.IsEnabled = true;
            }
            state.SelectionChanged += async (_, _) =>
            {
                if ((state.SelectedItem as ComboBoxItem)?.Tag is not string chosen || chosen == task.Status) return;
                await ChangeAtOnce(new("task_change", id, TasksRepo, Task: task.Id, Status: chosen),
                    () => task = task with { Status = chosen },
                    () => state.SelectedItem = state.Items.OfType<ComboBoxItem>().First(i => (string)i.Tag == task.Status),
                    $"Статус: «{TaskStatusName(chosen)}».");
            };
            assignee.SelectionChanged += async (_, _) =>
            {
                string chosen = (assignee.SelectedItem as ComboBoxItem)?.Tag as string ?? "";
                if (chosen == (task.Assignee ?? "")) return;
                await ChangeAtOnce(new("task_change", id, TasksRepo, Task: task.Id, Assignee: chosen),
                    () => task = task with { Assignee = chosen.Length > 0 ? chosen : null },
                    () => assignee.SelectedItem = assignee.Items.OfType<ComboBoxItem>().FirstOrDefault(i => ((i.Tag as string) ?? "") == (task.Assignee ?? "")),
                    chosen.Length > 0 ? $"Исполнитель: {(assignee.SelectedItem as ComboBoxItem)?.Content}." : "Без исполнителя.");
            };
            send.Click += async (_, _) =>
            {
                string said = reply.Text.Trim();
                if (said.Length == 0) return;
                send.IsEnabled = false;
                if (await Write(new("task_comment", id, TasksRepo, Task: task.Id, Text: reply.Text)) is { } failed)
                    Told($"Комментарий не отправлен: {failed}", failed: true);
                else
                {
                    AddComment("Вы", DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(), said);
                    discussion.Text = $"Обсуждение · {comments.Children.Count}";
                    reply.Text = string.Empty;
                }
                send.IsEnabled = true;
            };
            // «Сохранить» writes with the dialog open: on a failure it stays, with what is typed and why.
            // Only what changed goes: a field left alone stays as another person may be setting it.
            dialog.PrimaryButtonClick += async (_, e) =>
            {
                var wait = e.GetDeferral();
                try
                {
                    if (name.Text.Trim().Length == 0)
                    {
                        Told("У задачи должно быть название.", failed: true);
                        e.Cancel = true;
                        return;
                    }
                    string? newTitle = name.Text.Trim() != task.Title ? name.Text : null;
                    List<string> newTags = SplitList(tags.Text), newLinks = SplitList(links.Text);
                    bool tagsChanged = !newTags.SequenceEqual(task.Labels), linksChanged = !newLinks.SequenceEqual(task.Links);
                    if (newTitle is not null || tagsChanged || linksChanged)
                    {
                        if (await Write(new("task_change", id, TasksRepo, Task: task.Id, TaskTitle: newTitle,
                                Labels: tagsChanged ? newTags : null, Links: linksChanged ? newLinks : null)) is { } failed)
                        {
                            Told($"Не сохранено: {failed}", failed: true);
                            e.Cancel = true;
                            return;
                        }
                        // What is written is the task now: a failure below asks to save only the rest.
                        task = task with { Title = name.Text.Trim(), Labels = newTags, Links = newLinks };
                    }
                    if (text.Text.Trim() != task.Body.Trim())
                    {
                        if (await Write(new("task_describe", id, TasksRepo, Task: task.Id, Body: text.Text)) is { } failed)
                        {
                            Told(newTitle is not null || tagsChanged || linksChanged
                                ? $"Название, метки и файлы сохранены, описание — нет: {failed}"
                                : $"Описание не сохранено: {failed}", failed: true);
                            Track();
                            e.Cancel = true;
                            return;
                        }
                    }
                    Say($"#{task.Number} сохранена.");
                }
                finally { wait.Complete(); }
            };
            // Closing over anything not saved asks every time, in the dialog: an unsaved field or an unsent
            // comment. «Закрыть без сохранения» is the way out without them.
            bool discard = false;
            Button leave = PlainButton("Закрыть без сохранения", false, null);
            talkDoor.Click += (_, _) => { toTalk = true; dialog.Hide(); };
            dialog.Closing += (_, e) =>
            {
                if (e.Result == ContentDialogResult.Primary || discard) return;
                bool unsent = reply.Text.Trim().Length > 0;
                if (!Edited() && !unsent) return;
                toTalk = false;
                e.Cancel = true;
                note.Message = Edited() && unsent ? "Есть несохранённые изменения и неотправленный комментарий."
                    : unsent ? "Комментарий не отправлен." : "Изменения не сохранены.";
                note.Severity = InfoBarSeverity.Warning;
                note.ActionButton = leave;
                note.IsOpen = true;
            };
            leave.Click += (_, _) => { discard = true; dialog.Hide(); };
            note.Closed += (_, _) => note.ActionButton = null;
            dialogOpen = true;
            pairWatch.Start();
            try { await dialog.ShowAsync(); }
            finally { dialogOpen = false; pairWatch.Stop(); }
            if (toTalk && Live()) _ = ShowTaskTalkAsync(task.Id, $"#{task.Number} {task.Title}");
        }

        refresh.Click += (_, _) => _ = Reload();
        Draw();
        redraw = Draw;
        frame.SizeChanged += (_, _) =>
        {
            if (tasksViewChosen is null && board is { Enabled: true } && AsList() != shownAsList) Draw();
        };
        await Reload();
        if (openTask is not null && Live()) _ = OpenTask(openTask);
        // Others' changes come in by themselves: the board is read again while it is open.
        DispatcherTimer timer = new() { Interval = TimeSpan.FromSeconds(5) };
        timer.Tick += async (_, _) =>
        {
            if (!Live()) { timer.Stop(); return; }
            if (!busy && !dialogOpen && !dragging) await Reload();
        };
        timer.Start();
    }

    // A column of the board as a place a carried card goes: the line where it would land (at the
    // pointer, read through the function), taking the line away, and the drop.
    private sealed record DropColumn(
        Grid Column,
        Action<Func<UIElement, global::Windows.Foundation.Point>, string> Hover,
        Action Unmark,
        Func<Func<UIElement, global::Windows.Foundation.Point>, string, Task> Drop);

    // «Проработать вдвоём»: where the two agents are, what they propose, the remarks, every round;
    // `close` is there while it goes.
    private FrameworkElement PairBlock(WorkspacePair work, Action? close)
    {
        StackPanel block = new() { Spacing = 6 };
        string two = string.Join(" и ", work.With);
        block.Children.Add(SectionLabel($"Проработка вдвоём · {two}"));
        (string chip, string tone, string what) = work.Status switch
        {
            "agreed" => ("Черновик согласован", "Success", $"Агенты согласовали черновик на раунде {work.Round} — нужна проверка человеком. Перенесите нужное в описание задачи."),
            "deadlock" => ("Тупик", "Critical", "Два раунда без изменений. Решите сами по черновикам ниже или запустите проработку снова с уточнённой темой."),
            "limit" => ("Лимит раундов", "Caution", $"За {work.RoundsMax} раундов согласия нет. Решите сами по черновику и замечаниям или запустите снова."),
            "closed" => ("Завершена", "Neutral", "Проработку завершили до согласия."),
            _ => ("Идёт", "Accent", work.Round == 0 ? $"Агенты пишут первые черновики · раундов не больше {work.RoundsMax}" : $"Раунд {work.Round} из {work.RoundsMax}"),
        };
        block.Children.Add(StateLine(chip, tone, what));
        if (work.Topic.Length > 0) block.Children.Add(Caption($"Тема: {work.Topic}"));
        if (work.Drafts.LastOrDefault() is { } latest)
        {
            block.Children.Add(SectionLabel($"Предложение · {latest.AuthorName}, раунд {latest.Round}"));
            block.Children.Add(new TextBlock { Text = latest.Text, TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true });
        }
        if (work.Remarks.Count > 0)
        {
            int taken = work.Remarks.Count(r => r.Taken == true), refused = work.Remarks.Count(r => r.Taken == false), open = work.Remarks.Count(r => r.Taken is null);
            block.Children.Add(SectionLabel($"Замечания · принято {taken}, не принято {refused}, без ответа {open}"));
            // The ones still in question first; the taken ones too, to see what changed and why.
            foreach (WorkspacePairRemark remark in work.Remarks.OrderBy(r => r.Taken == true))
            {
                string answer = remark.Taken switch
                {
                    null => "без ответа",
                    true => string.IsNullOrEmpty(remark.AnswerWhy) ? "принято" : $"принято: {remark.AnswerWhy}",
                    false => $"не принято: {remark.AnswerWhy}",
                };
                block.Children.Add(Caption($"{remark.AuthorName}: {remark.What} — {answer}"));
            }
        }
        if (work.Drafts.Count > 1)
        {
            StackPanel rounds = new() { Spacing = 8 };
            foreach (WorkspacePairDraft draft in work.Drafts)
            {
                rounds.Children.Add(Caption($"Раунд {draft.Round} · {draft.AuthorName} · {MainWindowPresentation.FormatMessageTime(draft.AtUnixMs)}"));
                rounds.Children.Add(new TextBlock { Text = draft.Text, TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true });
            }
            block.Children.Add(new Expander { Header = $"Все черновики · {work.Drafts.Count}", Content = rounds, HorizontalAlignment = HorizontalAlignment.Stretch, HorizontalContentAlignment = HorizontalAlignment.Stretch });
        }
        if (close is not null) block.Children.Add(PlainButton("Завершить проработку", false, close));
        return block;
    }

    // Everything a card shows: the board is drawn again only when this changes.
    private static string BoardSignature(WorkspaceTasks board) =>
        board.Enabled + "|" + string.Join(";", board.Tasks.Select(t => $"{t.Id},{t.Status},{t.Title},{t.Assignee},{t.AssigneeName},{t.Comments.Count},{t.Conflict},{string.Join("/", t.Labels)},{t.UpdatedUnixMs},{t.Place},{t.Pair?.Status}"))
        + "|" + string.Join(";", board.Members.Select(m => m.AccountId + m.Name));

    private static ComboBox AssigneeBox(WorkspaceTasks board, string? chosen)
    {
        ComboBox box = new() { Header = "Исполнитель", MinWidth = 200 };
        box.Items.Add(new ComboBoxItem { Content = "Без исполнителя", Tag = "" });
        foreach (WorkspaceTaskMember member in board.Members.OrderByDescending(m => m.IsMe).ThenBy(m => m.Name))
            box.Items.Add(new ComboBoxItem { Content = member.IsMe ? $"{member.Name} (вы)" : member.Name, Tag = member.AccountId });
        box.SelectedItem = box.Items.OfType<ComboBoxItem>().FirstOrDefault(i => (string)i.Tag == (chosen ?? "")) ?? box.Items[0];
        return box;
    }

    // «a, b» or one per line.
    private static List<string> SplitList(string text) =>
        text.Split([',', '\r', '\n'], StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries).ToList();
}
