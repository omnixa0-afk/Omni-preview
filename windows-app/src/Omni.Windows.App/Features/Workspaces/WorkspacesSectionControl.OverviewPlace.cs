using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Omni.Windows.App.Presentation;

namespace Omni.Windows.App.Features.Workspaces;

/// «Обзор», the first place of a workspace (docs/decisions/omni-app-ux.md R7): what waits for this person
/// wherever it comes from (a question, a check, a project's trouble, their tasks), and beside it what
/// others did. The cards are the agents' page's: the same buttons, the same commands.
public sealed partial class WorkspacesSectionControl
{
    private const string OverviewPlace = "#overview";

    internal async Task ShowOverviewAsync()
    {
        if (bridge is null || groupId is null) return;
        RustBridge core = bridge; string id = groupId;
        if (!ProjectOpen) cardsOffset = WorkspaceCardsScroll.VerticalOffset;
        int page = ++projectTicket;
        projectRepo = OverviewPlace;
        projectBeforeTasks = null;
        codeRepo = null;
        ShowSwitcher(Place.Overview);
        SetPlaceBar();
        bool Live() => page == projectTicket;
        bool busy = false;
        CardHost cards = null!;
        WorkspaceAgents? place = null;
        WorkspaceTasks? board = null;
        string drawn = "\0";

        StackPanel main = new() { Spacing = 12 };
        StackPanel side = new() { Spacing = 12 };
        Grid areas = TwoAreas(main, side);
        areas.Margin = new Thickness(Gutter, 16, Gutter, 16);
        WorkspaceProjectHost.Children.Clear();
        WorkspaceProjectHost.Children.Add(new ScrollViewer { Content = areas, VerticalScrollBarVisibility = ScrollBarVisibility.Auto });
        WorkspaceCardsScroll.Visibility = Visibility.Collapsed;
        WorkspaceProjectHost.Visibility = Visibility.Visible;

        void Draw()
        {
            string fresh = (place is null ? "" : AgentsSignature(place)) + "#" + (board is null ? "" : BoardSignature(board)) + "#" + (screen is null ? "" : Signature(screen));
            if (fresh == drawn) return;
            drawn = fresh;
            main.Children.Clear();
            side.Children.Clear();

            List<WorkspaceAgentAsk> asks = place?.Asks.Where(a => a.CanAnswer).ToList() ?? [];
            if (asks.Count > 0)
            {
                main.Children.Add(SectionTitle($"Ждёт вашего решения · {asks.Count}", first: main.Children.Count == 0));
                foreach (WorkspaceAgentAsk ask in asks) main.Children.Add(Card(WithTalkLink(AskCard(cards, ask), ask.Thread)));
            }

            List<WorkspaceReviewAsk> checks = (place?.ReviewAsks ?? []).Where(r => r.CanReview).ToList();
            if (checks.Count > 0)
            {
                main.Children.Add(SectionTitle($"Ждёт проверки · {checks.Count}", first: main.Children.Count == 0));
                foreach (WorkspaceReviewAsk ask in checks) main.Children.Add(Card(WithTalkLink(ReviewCard(cards, ask), ask.Thread)));
            }

            if (screen is not null && AttentionCard(screen) is { } attention) main.Children.Add(attention);

            string? me = board?.Members.FirstOrDefault(m => m.IsMe)?.AccountId;
            List<WorkspaceTask> mine = board is { Enabled: true } && me is not null
                ? board.Tasks.Where(t => t.Assignee == me && t.Status != "done").OrderBy(t => Array.FindIndex(TaskColumns, c => c.Status == t.Status)).ThenBy(t => t.Place).ToList()
                : [];
            if (mine.Count > 0)
            {
                main.Children.Add(SectionTitle($"Мои задачи · {mine.Count}", first: main.Children.Count == 0));
                foreach (WorkspaceTask task in mine) main.Children.Add(MyTaskRow(task));
            }

            if (main.Children.Count == 0 && place is not null && screen is not null)
            {
                bool noProjects = !screen.Repos.Any(r => !r.IsTasks);
                main.Children.Add(new TextBlock { Text = "Сейчас от вас ничего не ждут", Style = CardTitle });
                main.Children.Add(Caption(noProjects
                    ? "В группе ещё нет проекта. Добавьте папку, и участники увидят её здесь."
                    : "Здесь появятся вопросы агентов, изменения на проверку и ваши задачи."));
                if (noProjects) main.Children.Add(PlainButton("Добавить папку", true, () => _ = BindFolderAsync()));
            }

            if (screen is not null) side.Children.Add(NewsArea(screen));
        }

        FrameworkElement MyTaskRow(WorkspaceTask task)
        {
            StackPanel text = new() { Spacing = 1 };
            text.Children.Add(new TextBlock { Text = $"#{task.Number} {task.Title}", TextTrimming = TextTrimming.CharacterEllipsis });
            text.Children.Add(Caption(TaskStatusName(task.Status)));
            Button row = new() { Content = text, Style = (Style)Resources["WorkspaceNewsRowStyle"] };
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(row, $"#{task.Number} {task.Title}");
            row.Click += (_, _) => _ = ShowTasksAsync(task.Id);
            return row;
        }

        async Task<bool> Run(WorkspaceAgentsRequest request, string done)
        {
            if (busy) return false;
            busy = true;
            try
            {
                WorkspaceAgents after = await Task.Run(() => core.WorkspaceAgents(request));
                if (!Live()) return true;
                place = after;
                Draw();
                SetStatus(done, StatusKind.Done);
                return true;
            }
            catch (Exception ex)
            {
                if (Live()) SetStatus(WorkspaceErrorText.ForCoreMessage(ex.Message), StatusKind.Failed);
                return false;
            }
            finally { busy = false; }
        }

        async Task Reload()
        {
            busy = true;
            try
            {
                (WorkspaceAgents? agents, WorkspaceTasks? tasks) = await Task.Run(() =>
                {
                    WorkspaceAgents? readAgents = null;
                    WorkspaceTasks? readTasks = null;
                    // Either part may fail on its own: what was read is still worth showing.
                    try { readAgents = core.WorkspaceAgents(new("view", id)); }
                    catch (Exception ex) { AppTrace.Write("workspace/overview", $"agents: {ex.Message}"); }
                    try { readTasks = core.WorkspaceFiles(new("tasks", id, TasksRepo)).Tasks; }
                    catch (Exception ex) { AppTrace.Write("workspace/overview", $"tasks: {ex.Message}"); }
                    return (readAgents, readTasks);
                });
                if (!Live()) return;
                place = agents ?? place;
                board = tasks ?? board;
                Draw();
            }
            finally { busy = false; }
        }

        cards = new(core, id, Run);
        projectScreenChanged = _ => { if (Live()) Draw(); };
        Draw();
        await Reload();

        DispatcherTimer timer = new() { Interval = TimeSpan.FromSeconds(4) };
        timer.Tick += async (_, _) =>
        {
            if (!Live()) { timer.Stop(); return; }
            // It is the default place: left open behind another section, it must not keep reading.
            if (Visibility == Visibility.Visible && !busy && !cards.DialogOpen) await Reload();
        };
        timer.Start();
    }
}
