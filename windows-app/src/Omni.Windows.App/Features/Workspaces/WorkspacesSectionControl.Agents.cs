using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Threading.Tasks;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Omni.Windows.App.Presentation;

namespace Omni.Windows.App.Features.Workspaces;

/// The agents' place of a workspace (docs/decisions/omni-workspace-git.md section 5): what waits for our
/// decision first, then the agents with what each is doing, then what happened. Connecting one of ours,
/// giving it work and stopping them all start here. An agent is a profile of its own run by `omni-mcp`;
/// the place only shows and answers.
public sealed partial class WorkspacesSectionControl
{
    // The agents' place has no project: this stands for it where the open place is named.
    private const string AgentsPlace = "#agents";
    // History lines shown before «Вся история».
    private const int HistoryShown = 6;

    internal async Task ShowAgentsAsync()
    {
        if (bridge is null || groupId is null) return;
        RustBridge core = bridge; string id = groupId;
        if (!ProjectOpen) cardsOffset = WorkspaceCardsScroll.VerticalOffset;
        int page = ++projectTicket;
        projectRepo = AgentsPlace;
        projectScreenChanged = null;
        ShowSwitcher(Place.Agents);
        bool Live() => page == projectTicket;
        bool busy = false, wholeHistory = false;
        // The cards run through the page's Run, which redraws them: set once the page is built.
        CardHost cards = null!;
        WorkspaceAgents? place = null;
        string drawn = string.Empty;

        // What waits and the agents on the left, the history beside them (under them on a narrow
        // window); the place's buttons on the places' strip.
        StackPanel sheet = new() { Spacing = 12 };
        StackPanel side = new() { Spacing = 12 };
        Grid column = TwoAreas(sheet, side);
        column.Margin = new Thickness(Gutter, 16, Gutter, 16);

        Button connect = PlainButton("Подключить агента", true, null);
        Button stop = PlainButton("Остановить всех агентов", false, null);
        SetPlaceBar(connect, stop);

        void Say(string text, StatusKind kind = StatusKind.Done)
        {
            if (Live()) SetStatus(text, kind);
        }

        StackPanel body = new() { Spacing = 12 };
        sheet.Children.Add(body);
        ScrollViewer scroll = new() { Content = column, VerticalScrollBarVisibility = ScrollBarVisibility.Auto };

        WorkspaceProjectHost.Children.Clear();
        WorkspaceProjectHost.Children.Add(scroll);
        WorkspaceCardsScroll.Visibility = Visibility.Collapsed;
        WorkspaceProjectHost.Visibility = Visibility.Visible;
        cards = new(core, id, (request, done) => Run(request, done));

        void Take(WorkspaceAgents after)
        {
            if (!Live()) return;
            place = after;
            Draw();
        }

        async Task<bool> Run(WorkspaceAgentsRequest request, string done)
        {
            if (busy) return false;
            busy = true;
            try
            {
                Take(await Task.Run(() => core.WorkspaceAgents(request)));
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

        async Task Reload()
        {
            try
            {
                WorkspaceAgents fresh = await Task.Run(() => core.WorkspaceAgents(new("view", id)));
                if (!Live()) return;
                place = fresh;
                if (AgentsSignature(fresh) != drawn) Draw();
            }
            catch (Exception ex) { Say($"Не удалось прочитать агентов: {WorkspaceErrorText.ForCoreMessage(ex.Message)}", StatusKind.Failed); }
        }

        void Draw()
        {
            body.Children.Clear();
            side.Children.Clear();
            if (place is null) return;
            drawn = AgentsSignature(place);
            stop.Visibility = place.CanStop && !place.Stopped && place.Agents.Any(a => a.InGroup) ? Visibility.Visible : Visibility.Collapsed;

            if (place.Stopped)
            {
                StackPanel stopped = new() { Spacing = 8 };
                string who = string.IsNullOrEmpty(place.StoppedBy) ? "Агенты остановлены." : $"Агентов остановил {place.StoppedBy}.";
                stopped.Children.Add(StateLine("Остановлены", "Critical", $"{who} В этом workspace они ничего не пишут и не меняют, пока их не пустят. Сами приложения агентов при этом работают."));
                if (place.CanStop)
                    stopped.Children.Add(PlainButton("Пустить агентов", false, () => _ = Run(new("stop", id, Stopped: false), "Агенты снова могут работать в workspace.")));
                body.Children.Add(Card(stopped));
            }

            // What waits for our decision first: nothing an agent waits for should be below the fold.
            List<WorkspaceAgentAsk> mine = place.Asks.Where(a => a.CanAnswer).ToList();
            if (mine.Count > 0)
            {
                body.Children.Add(SectionTitle($"Ждёт вашего решения · {mine.Count}", first: body.Children.Count == 0));
                foreach (WorkspaceAgentAsk ask in mine) body.Children.Add(Card(WithTalkLink(AskCard(cards, ask), ask.Thread)));
            }

            // Branches asked to be checked: a person gives the verdict too, not only another agent.
            List<WorkspaceReviewAsk> toCheck = (place.ReviewAsks ?? []).Where(r => r.CanReview).ToList();
            if (toCheck.Count > 0)
            {
                body.Children.Add(SectionTitle($"Ждёт проверки · {toCheck.Count}", first: body.Children.Count == 0));
                foreach (WorkspaceReviewAsk ask in toCheck) body.Children.Add(Card(WithTalkLink(ReviewCard(cards, ask), ask.Thread)));
            }

            body.Children.Add(SectionTitle("Агенты", first: body.Children.Count == 0));
            if (place.Agents.Count == 0) body.Children.Add(Card(EmptyAgents()));
            Features.Shared.PageColumns agents = new() { Spacing = 8, ColumnGap = 12, MinColumn = 480, EachChild = true };
            foreach (WorkspaceAgent agent in place.Agents.OrderByDescending(a => a.Mine).ThenBy(a => a.Name)) agents.Children.Add(AgentRow(agent));
            body.Children.Add(agents);

            List<(long At, FrameworkElement Line)> history = [];
            foreach (WorkspaceAgentAsk ask in place.Asks.Where(a => a.Answer is not null))
            {
                long at = ask.AnsweredUnixMs > 0 ? ask.AnsweredUnixMs : ask.AtUnixMs;
                history.Add((at, HistoryLine(at, AnsweredWhat(ask), Answered(ask), ask.Reason ?? "")));
            }
            foreach (WorkspaceAgentReview review in place.Reviews)
                history.Add((review.AtUnixMs, HistoryLine(review.AtUnixMs, $"{review.ReviewerName} проверил(а) изменения в «{review.Repo}»",
                    review.Accepted && review.TestsPassed ? ("Принято", "Success") : review.Accepted ? ("Тесты не прошли", "Caution") : ("Нужны правки", "Caution"),
                    review.Notes)));
            if (history.Count > 0)
            {
                side.Children.Add(SectionTitle("История", first: true));
                List<FrameworkElement> lines = history.OrderByDescending(h => h.At).Select(h => h.Line).ToList();
                foreach (FrameworkElement line in wholeHistory ? lines : lines.Take(HistoryShown)) side.Children.Add(line);
                if (lines.Count > HistoryShown)
                {
                    HyperlinkButton more = new() { Content = wholeHistory ? "Свернуть" : $"Вся история · {lines.Count}", Padding = new Thickness(0) };
                    more.Click += (_, _) => { wholeHistory = !wholeHistory; Draw(); };
                    side.Children.Add(more);
                }
            }
        }

        FrameworkElement EmptyAgents()
        {
            StackPanel empty = new() { Spacing = 8 };
            empty.Children.Add(new TextBlock { Text = "Агентов пока нет", Style = CardTitle });
            empty.Children.Add(Body("Агент — ваш помощник-программа, которая работает в workspace как участник: берёт задачи, пишет в обсуждение, делает свои ветки и правки. Задания ему даёте вы, а в общую версию проекта его работа попадает только после вашего «Применить»."));
            empty.Children.Add(PlainButton("Подключить агента", true, () => _ = ConnectAsync()));
            return empty;
        }

        FrameworkElement AgentRow(WorkspaceAgent agent)
        {
            // One compact row: who and in what state on the left with the latest word under it, the actions
            // at the right. A page of four agents fits a 720 window (docs/decisions/omni-app-ux.md R7).
            Grid row = new() { ColumnSpacing = 12 };
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            StackPanel text = new() { Spacing = 2 };
            StackPanel title = new() { Orientation = Orientation.Horizontal, Spacing = 10 };
            title.Children.Add(new TextBlock { Text = agent.Name, Style = CardTitle, VerticalAlignment = VerticalAlignment.Center });
            text.Children.Add(title);
            string whose = agent.Mine ? "ваш агент" : agent.Confirmed && !string.IsNullOrEmpty(agent.OwnerName) ? $"агент {agent.OwnerName}" : "агент, хозяин не подтвердил";
            whose += agent.MembersCommand ? " · задания дают все участники" : agent.Mine ? " · задания даёте только вы" : "";
            text.Children.Add(Caption(whose));
            foreach (string line in Doing(agent))
            {
                TextBlock said = Body(line);
                said.MaxLines = 1;
                said.TextTrimming = TextTrimming.CharacterEllipsis;
                ToolTipService.SetToolTip(said, line);
                text.Children.Add(said);
            }
            // Its server runs, but no model waits for lines: what was written to it lies unread.
            if (agent.Mine && agent.Online && !agent.Listening && !place!.Stopped)
                text.Children.Add(Caption($"Сейчас его никто не слушает: откройте чат с {agent.Name} в его приложении и напишите «Подключись к Omni и жди, когда тебя позовут»."));
            row.Children.Add(text);

            (string chip, string tone) = agent.State switch
            {
                "invited" => ("Приглашён", "Accent"),
                "connecting" => ("Ждём связи", "Caution"),
                _ when place!.Stopped => ("Остановлен", "Critical"),
                _ when agent.WaitingAnswer > 0 => ("Ждёт ответа", "Caution"),
                _ when agent.Listening => ("Слушает", "Success"),
                _ => agent.Online ? ("Не слушает", "Caution") : ("Не в сети", "Neutral"),
            };
            Border state = Chip(chip, tone);
            state.VerticalAlignment = VerticalAlignment.Center;
            title.Children.Add(state);

            if (agent.Mine)
            {
                StackPanel actions = new() { Orientation = Orientation.Horizontal, Spacing = 8 };
                if (agent.InGroup && !place!.Stopped)
                    actions.Children.Add(PlainButton("Дать работу…", false, () => _ = GiveWorkAsync(agent)));
                // Unlinking is rare and takes the agent away: in the card's «…», not beside «Дать работу…»
                // as its equal (docs/decisions/omni-app-ux.md R7).
                MenuFlyoutItem unlink = new() { Text = "Отвязать от меня…" };
                MenuFlyout more = new();
                more.Items.Add(unlink);
                Button moreButton = new()
                {
                    Style = (Style)Application.Current.Resources["GhostIconButtonStyle"],
                    Content = new FontIcon { Glyph = "\uE712", FontSize = 16 },
                    Flyout = more,
                };
                Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(moreButton, "Ещё действия");
                ToolTipService.SetToolTip(moreButton, "Ещё действия");
                unlink.Click += async (_, _) =>
                {
                    if (cards.DialogOpen) return;
                    cards.DialogOpen = true;
                    try
                    {
                        ContentDialog confirm = new()
                        {
                            XamlRoot = XamlRoot, RequestedTheme = MainWindowPresentation.ActiveTheme,
                            Title = $"Отвязать {agent.Name} от себя?",
                            Content = Body("Он перестанет быть вашим агентом: вы больше не даёте ему заданий и не отвечаете на его вопросы. Участником workspace он останется — убрать его может админ, как любого участника."),
                            PrimaryButtonText = "Отвязать",
                            CloseButtonText = "Отмена",
                            DefaultButton = ContentDialogButton.Close,
                        };
                        if (await confirm.ShowAsync() == ContentDialogResult.Primary)
                            await Run(new("forget", id, AccountId: agent.AccountId), $"{agent.Name} больше не ваш агент.");
                    }
                    finally { cards.DialogOpen = false; }
                };
                actions.Children.Add(moreButton);
                actions.VerticalAlignment = VerticalAlignment.Center;
                Grid.SetColumn(actions, 1);
                row.Children.Add(actions);
            }
            return Card(row);
        }

        // «Дать работу»: a task of the board or a few words; it goes into the work's discussion as a
        // command naming the agent, and that discussion opens after it.
        async Task GiveWorkAsync(WorkspaceAgent agent)
        {
            if (cards.DialogOpen) return;
            cards.DialogOpen = true;
            try
            {
                List<WorkspaceTask> open = [];
                string? tasksFailed = null;
                try { open = (await Task.Run(() => core.WorkspaceFiles(new("tasks", id, TasksRepo)))).Tasks?.Tasks.Where(t => t.Status != "done").OrderBy(t => t.Number).ToList() ?? []; }
                catch (Exception ex) { tasksFailed = WorkspaceErrorText.ForCoreMessage(ex.Message); }
                StackPanel form = new() { Spacing = 10, MinWidth = 460 };
                form.Children.Add(Body($"Поручение уйдёт в обсуждение этой работы — там видно, что {agent.Name} отвечает, и там же можно ему написать. Обсуждение видят все участники workspace."));
                if (!agent.Listening)
                    form.Children.Add(Caption($"{agent.Name} сейчас не слушает: поручение дождётся его в обсуждении, он прочитает его, когда его снова подключат."));
                if (tasksFailed is not null)
                    form.Children.Add(Caption($"Задачи прочитать не удалось ({tasksFailed}) — можно поручить словами."));
                ComboBox task = new() { Header = "Задача", HorizontalAlignment = HorizontalAlignment.Stretch };
                task.Items.Add(new ComboBoxItem { Content = "Без задачи", Tag = "" });
                foreach (WorkspaceTask t in open) task.Items.Add(new ComboBoxItem { Content = $"#{t.Number} {t.Title}", Tag = t.Id });
                task.SelectedIndex = 0;
                TextBox what = new() { Header = "Что сделать", PlaceholderText = "Например: проверь размеры крышки по чертежу", AcceptsReturn = true, TextWrapping = TextWrapping.Wrap, MinHeight = 80 };
                TextBlock preview = Caption("");
                InfoBar error = new() { IsOpen = false, IsClosable = false, Severity = InfoBarSeverity.Error };
                form.Children.Add(task);
                form.Children.Add(what);
                form.Children.Add(preview);
                form.Children.Add(error);
                ContentDialog dialog = new()
                {
                    XamlRoot = XamlRoot, RequestedTheme = MainWindowPresentation.ActiveTheme,
                    Title = $"Дать работу {agent.Name}", Content = form,
                    PrimaryButtonText = "Отправить", CloseButtonText = "Отмена", DefaultButton = ContentDialogButton.Primary,
                };
                string? TaskId() => (task.SelectedItem as ComboBoxItem)?.Tag is string { Length: > 0 } t ? t : null;
                string? opened = null;
                void Update()
                {
                    string line = $"@{agent.Name}";
                    if (task.SelectedItem is ComboBoxItem { Tag: string { Length: > 0 } } item) line += $" возьми задачу {item.Content}" + (what.Text.Trim().Length > 0 ? ":" : "");
                    if (what.Text.Trim().Length > 0) line += " " + what.Text.Trim();
                    preview.Text = $"В обсуждение уйдёт: {line}" + (TaskId() is not null ? $"\nЗадача станет его — он её исполнитель." : "");
                    dialog.IsPrimaryButtonEnabled = TaskId() is not null || what.Text.Trim().Length > 0;
                }
                task.SelectionChanged += (_, _) => Update();
                what.TextChanged += (_, _) => Update();
                Update();
                // The dialog stays open on a failure, with what was typed.
                dialog.PrimaryButtonClick += async (_, e) =>
                {
                    var wait = e.GetDeferral();
                    try
                    {
                        // The boxes are read here, on the UI thread: from the pool their Text throws.
                        WorkspaceAgentsRequest give = new("give", id, AccountId: agent.AccountId, Text: what.Text.Trim(), Task: TaskId());
                        WorkspaceAgents after = await Task.Run(() => core.WorkspaceAgents(give));
                        opened = after.Opened;
                        Take(after);
                        Say(agent.Listening
                            ? $"Задание отправлено {agent.Name}. Его ответ — здесь, в обсуждении."
                            : $"Задание отправлено {agent.Name}: он прочитает его, когда его снова подключат.");
                    }
                    catch (Exception ex)
                    {
                        AppTrace.Write("workspace/agent-give", $"give FAILED: {ex}");
                        error.Message = WorkspaceErrorText.ForCoreMessage(ex.Message);
                        error.IsOpen = true;
                        e.Cancel = true;
                    }
                    finally { wait.Complete(); }
                };
                await dialog.ShowAsync();
                if (opened is not null && Live()) _ = ShowTalkAsync(opened);
            }
            finally { cards.DialogOpen = false; }
        }

        FrameworkElement HistoryLine(long at, string what, (string Text, string Tone) verdict, string notes)
        {
            Grid line = new() { ColumnSpacing = 10 };
            line.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            line.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            StackPanel text = new() { Spacing = 2 };
            text.Children.Add(new TextBlock { Text = what, TextWrapping = TextWrapping.Wrap });
            text.Children.Add(Caption(notes.Length > 0 ? $"{MainWindowPresentation.FormatMessageTime(at)} · {notes}" : MainWindowPresentation.FormatMessageTime(at)));
            line.Children.Add(text);
            Border chip = Chip(verdict.Text, verdict.Tone);
            chip.VerticalAlignment = VerticalAlignment.Top;
            Grid.SetColumn(chip, 1);
            line.Children.Add(chip);
            return line;
        }

        async Task ConnectAsync()
        {
            if (cards.DialogOpen || place is null) return;
            cards.DialogOpen = true;
            try { await ConnectAgentAsync(core, id, place.MyAccount, after => { Take(after); }, Say); }
            finally { cards.DialogOpen = false; }
        }

        stop.Click += async (_, _) =>
        {
            if (cards.DialogOpen) return;
            cards.DialogOpen = true;
            try
            {
                ContentDialog confirm = new()
                {
                    XamlRoot = XamlRoot, RequestedTheme = MainWindowPresentation.ActiveTheme,
                    Title = "Остановить всех агентов?",
                    Content = Body("Агенты этого workspace перестанут писать, менять задачи и файлы и публиковать ветки, пока их не пустят. Сами приложения агентов продолжат работать — остановка действует только здесь. Участники увидят, кто остановил."),
                    PrimaryButtonText = "Остановить",
                    CloseButtonText = "Отмена",
                    DefaultButton = ContentDialogButton.Close,
                };
                if (await confirm.ShowAsync() == ContentDialogResult.Primary)
                    await Run(new("stop", id, Stopped: true), "Агенты остановлены.");
            }
            finally { cards.DialogOpen = false; }
        };

        connect.Click += (_, _) => _ = ConnectAsync();

        Draw();
        await Reload();
        DispatcherTimer timer = new() { Interval = TimeSpan.FromSeconds(4) };
        timer.Tick += async (_, _) =>
        {
            if (!Live()) { timer.Stop(); return; }
            if (!busy && !cards.DialogOpen) await Reload();
        };
        timer.Start();
    }

    // What an agent is doing, in a line or two: waiting for us, its tasks, its latest word.
    private static IEnumerable<string> Doing(WorkspaceAgent agent)
    {
        switch (agent.State)
        {
            case "connecting":
                yield return "Ждём связи с ним: приложение агента должно быть запущено.";
                yield break;
            case "invited":
                yield return "Приглашён в workspace — агент примет приглашение сам.";
                yield break;
        }
        if (agent.WaitingAnswer > 0) yield return agent.WaitingAnswer == 1 ? "Ждёт вашего ответа — он выше." : $"Ждёт ваших ответов: {agent.WaitingAnswer} — они выше.";
        if (agent.Tasks is { Count: > 0 } tasks) yield return $"Задачи: {string.Join(", ", tasks)}";
        if (!string.IsNullOrEmpty(agent.LastSaid)) yield return $"Последнее: «{agent.LastSaid}» · {MainWindowPresentation.FormatMessageTime(agent.LastActiveUnixMs)}";
        else if (agent.Tasks is not { Count: > 0 }) yield return "Сейчас без задач — дайте ему работу.";
    }

    // «M calc.py» as a person reads it.
    private static string FileChange(string line)
    {
        string[] parts = line.Split('\t');
        if (parts.Length < 2) return line;
        string path = parts[^1];
        return parts[0][0] switch { 'A' => $"+ {path}", 'D' => $"− {path}", 'R' => $"{parts[1]} → {path}", _ => $"изменён {path}" };
    }

    private static string AnsweredWhat(WorkspaceAgentAsk ask) => (ask.Action, ask.Answer) switch
    {
        ("main", true) => $"Изменения {ask.AgentName} применены в общую версию «{ask.Repo}»",
        ("main", _) => $"Изменения {ask.AgentName} в «{ask.Repo}» возвращены на доработку",
        _ => $"{ask.AgentName} спросил(а): «{ask.Text}»",
    };

    private static (string, string) Answered(WorkspaceAgentAsk ask) => (ask.Action, ask.Answer) switch
    {
        ("main", true) => ("Применено", "Success"),
        ("main", _) => ("На доработке", "Caution"),
        (_, true) => ("Да", "Success"),
        _ => ("Нет", "Neutral"),
    };

    private static string AgentsSignature(WorkspaceAgents place) =>
        $"{place.Stopped}|{place.StoppedBy}|{place.CanStop}|"
        + string.Join(";", place.Agents.Select(a => $"{a.AccountId},{a.Name},{a.State},{a.Confirmed},{a.OwnerName},{a.MembersCommand},{a.Online},{a.Listening},{a.LastSaid},{a.LastActiveUnixMs},{a.WaitingAnswer},{string.Join("/", a.Tasks ?? [])}"))
        + "|" + string.Join(";", place.Asks.Select(a => $"{a.Id},{a.Answer},{a.Reason},{a.AnsweredUnixMs},{a.CanAnswer},{string.Join("/", a.AcceptedBy)},{string.Join("/", a.Commits ?? [])},{string.Join("/", a.Files ?? [])}"))
        + "|" + string.Join(";", place.Reviews.Select(r => $"{r.Oid},{r.ReviewerName},{r.Accepted},{r.TestsPassed},{r.Notes},{r.AtUnixMs}"))
        + "|" + string.Join(";", (place.ReviewAsks ?? []).Select(r => $"{r.Repo},{r.Oid},{r.Here},{r.CanReview},{string.Join("/", r.Commits)},{string.Join("/", r.Files)}"));

    /// The folder an agent named `name` keeps its own Omni in, on this computer.
    internal static string AgentFolder(string name)
    {
        string safe = new(name.Trim().Select(c => Path.GetInvalidFileNameChars().Contains(c) ? '_' : c).ToArray());
        string root = Environment.GetEnvironmentVariable("OMNI_AGENTS_DIR") is { Length: > 0 } custom
            ? custom
            : Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.MyDocuments), "Omni Agents");
        return Path.Combine(root, safe.Length > 0 ? safe : "agent");
    }

    /// What the agent's MCP settings run: this very Omni as `omni-mcp`.
    internal static string AgentCommandLine(string name, string owner) =>
        $"\"{Environment.ProcessPath}\" --run-helper mcp --dir \"{AgentFolder(name)}\" --name \"{name.Trim()}\" --owner {owner}";

    // The same server in the two settings formats agents' applications read.
    private static string AgentSettings(string format, string name, string owner)
    {
        string[] args = ["--run-helper", "mcp", "--dir", AgentFolder(name), "--name", name.Trim(), "--owner", owner];
        string exe = Environment.ProcessPath ?? "Omni.exe";
        static string Json(string s) => "\"" + s.Replace("\\", "\\\\").Replace("\"", "\\\"") + "\"";
        // A TOML basic string escapes as JSON does.
        static string Toml(string s) => Json(s);
        return format switch
        {
            // Without the last two lines a non-interactive run refuses every Omni tool («requires
            // approval»), and a long omni_wait (up to 600 s) outlives the default tool timeout. What
            // reaches main is still the owner's «Применить» in Omni.
            "toml" => $"[mcp_servers.omni]\ncommand = {Toml(exe)}\nargs = [{string.Join(", ", args.Select(Toml))}]\ndefault_tools_approval_mode = \"approve\"\ntool_timeout_sec = 660",
            "json" => $"{{\n  \"mcpServers\": {{\n    \"omni\": {{\n      \"command\": {Json(exe)},\n      \"args\": [{string.Join(", ", args.Select(Json))}]\n    }}\n  }}\n}}",
            _ => AgentCommandLine(name, owner),
        };
    }

    // The agents' applications Omni writes itself into on this computer (the core's add_to_app).
    private static readonly (string Tag, string Shown, string Name)[] AgentApps =
    [
        ("claude", "Claude (приложение Claude, раздел Code)", "Claude"),
        ("codex", "Codex", "Codex"),
        ("other", "Другое приложение с MCP", ""),
    ];

    /// Connecting an agent, step by step: its application, name and whose word it takes; where it runs;
    /// on this computer Claude and Codex get Omni written into their settings with one button, any other
    /// application gets the settings to copy; its code — read from its folder once it ran here, or
    /// pasted when it runs on another computer. The dialog stays open until the connection is asked for.
    private async Task ConnectAgentAsync(RustBridge core, string id, string owner, Action<WorkspaceAgents> done, Action<string, StatusKind> say)
    {
        StackPanel content = new() { Spacing = 12, Width = 560 };
        ComboBox app = new() { Header = "Приложение агента", HorizontalAlignment = HorizontalAlignment.Stretch };
        foreach ((string tag, string shown, _) in AgentApps) app.Items.Add(new ComboBoxItem { Content = shown, Tag = tag });
        app.SelectedIndex = 0;
        TextBox name = new() { Header = "Имя агента", PlaceholderText = "Например, Кодер", Text = AgentApps[0].Name };
        RadioButtons who = new() { Header = "Кто даёт ему задания", MaxColumns = 2 };
        who.Items.Add("Только я");
        who.Items.Add("Все участники workspace");
        who.SelectedIndex = 0;
        TextBlock whoNote = Caption("В общую версию проекта его работа всё равно попадает только после вашего «Применить».");
        RadioButtons where = new() { Header = "Где работает агент", MaxColumns = 2 };
        where.Items.Add("На этом компьютере");
        where.Items.Add("На другом компьютере");
        where.SelectedIndex = 0;

        TextBlock settingsTitle = SectionLabel("Настройка приложения агента");
        TextBlock settingsHow = Body("");
        ComboBox format = new() { Header = "Как приложение агента принимает MCP-серверы", HorizontalAlignment = HorizontalAlignment.Stretch };
        format.Items.Add(new ComboBoxItem { Content = "Файл config.toml (раздел mcp_servers)", Tag = "toml" });
        format.Items.Add(new ComboBoxItem { Content = "JSON-настройки (раздел mcpServers)", Tag = "json" });
        format.Items.Add(new ComboBoxItem { Content = "Команда запуска", Tag = "line" });
        format.SelectedIndex = 0;
        TextBox settings = new() { IsReadOnly = true, TextWrapping = TextWrapping.Wrap, AcceptsReturn = true, FontFamily = new Microsoft.UI.Xaml.Media.FontFamily("Consolas"), FontSize = 12 };
        Button copy = PlainButton("Копировать", false, null);
        Button add = PlainButton("Добавить в Claude", true, null);
        TextBlock added = Body("");
        added.IsTextSelectionEnabled = true;

        TextBlock stepsTitle = SectionLabel("Подключение");
        TextBlock stepCopied = Body("");
        TextBlock stepStarted = Body("");
        TextBox code = new() { Header = "Код агента", PlaceholderText = "Код из файла agent-code.txt в папке агента" };
        Expander help = new() { Header = "Агент не появляется?", HorizontalAlignment = HorizontalAlignment.Stretch, HorizontalContentAlignment = HorizontalAlignment.Stretch };
        StackPanel helpBody = new() { Spacing = 6 };
        helpBody.Children.Add(Body("• Настройка добавлена в приложение агента, и оно перезапущено после этого."));
        helpBody.Children.Add(Body("• В настройке тот же путь к Omni, что показан выше."));
        helpBody.Children.Add(Body("• При первом запуске агент создаёт свою папку и кладёт в неё файл agent-code.txt — проверьте, что он появился."));
        Button openFolder = PlainButton("Открыть папку агента", false, null);
        helpBody.Children.Add(openFolder);
        help.Content = helpBody;
        InfoBar error = new() { IsOpen = false, IsClosable = false, Severity = InfoBarSeverity.Error };

        foreach (UIElement e in new UIElement[] { app, name, who, whoNote, where, settingsTitle, settingsHow, add, added, format, settings, copy, stepsTitle, stepCopied, stepStarted, code, help, error })
            content.Children.Add(e);
        ContentDialog dialog = new()
        {
            XamlRoot = XamlRoot, RequestedTheme = MainWindowPresentation.ActiveTheme,
            Title = "Подключить агента",
            Content = new ScrollViewer { Content = content, MaxHeight = Math.Max(420, (XamlRoot?.Size.Height ?? 800) - 260), VerticalScrollBarVisibility = ScrollBarVisibility.Auto, Padding = new Thickness(0, 0, 12, 0) },
            PrimaryButtonText = "Подключить",
            CloseButtonText = "Отмена",
            DefaultButton = ContentDialogButton.Primary,
            IsPrimaryButtonEnabled = false,
        };
        // The content is wider than a dialog's default 548: the settings line reads whole.
        dialog.Resources["ContentDialogMaxWidth"] = 660.0;

        bool copied = false;
        string addedTo = string.Empty; // the settings file Omni was written into
        string found = string.Empty; // the code read from the agent's folder, not typed
        bool Here() => where.SelectedIndex == 0;
        string App() => (app.SelectedItem as ComboBoxItem)?.Tag as string ?? "other";
        string AppName() => App() == "codex" ? "Codex" : "Claude";
        // Claude and Codex on this computer: the button; anything else: the settings to copy.
        bool OneButton() => Here() && App() != "other";
        string Format() => (format.SelectedItem as ComboBoxItem)?.Tag as string ?? "toml";
        void Update()
        {
            bool named = name.Text.Trim().Length > 0;
            settingsHow.Text = OneButton()
                ? $"Omni впишет себя в настройки {AppName()} на этом компьютере — прежний файл настроек сохранится рядом. Остальные настройки {AppName()} не меняются."
                : Here()
                    ? "Добавьте Omni в настройки приложения агента как MCP-сервер и перезапустите приложение. Omni сам увидит первый запуск агента."
                    : "На том компьютере должен быть установлен Omni. Добавьте эту запись в настройки агента там, заменив путь к Omni и папку агента на тамошние. Запущенный агент положит в свою папку файл agent-code.txt — вставьте его содержимое ниже.";
            add.Content = $"Добавить в {AppName()}";
            add.Visibility = added.Visibility = OneButton() ? Visibility.Visible : Visibility.Collapsed;
            add.IsEnabled = named;
            added.Text = addedTo.Length > 0
                ? App() == "claude"
                    ? $"Готово: {addedTo}. Откройте новую сессию в разделе Code приложения Claude и напишите в ней: «Подключись к Omni и жди, когда тебя позовут»."
                    : $"Готово: {addedTo}. Перезапустите Codex, откройте новый чат и напишите ему: «Подключись к Omni и жди, когда тебя позовут»."
                : "";
            format.Visibility = settings.Visibility = copy.Visibility = OneButton() ? Visibility.Collapsed : Visibility.Visible;
            settings.Text = named ? AgentSettings(Format(), name.Text, owner) : "Сначала задайте имя агента.";
            copy.IsEnabled = named;
            stepCopied.Text = OneButton()
                ? (addedTo.Length > 0 ? $"✓ Omni добавлен в {AppName()}" : $"1. Добавьте Omni в {AppName()} кнопкой выше")
                : copied ? "✓ Настройка скопирована" : "1. Скопируйте настройку и добавьте её в приложение агента";
            stepStarted.Text = code.Text.Trim().Length > 0
                ? (found.Length > 0 && code.Text.Trim() == found ? "✓ Код агента найден в его папке — можно подключать" : "✓ Код агента вставлен")
                : Here() ? "2. Ждём первого запуска агента…" : "2. Вставьте код агента с того компьютера";
            code.Visibility = Here() && found.Length == 0 ? Visibility.Collapsed : Visibility.Visible;
            openFolder.IsEnabled = named;
            dialog.IsPrimaryButtonEnabled = named && code.Text.Trim().Length > 0;
        }
        name.TextChanged += (_, _) =>
        {
            // Another name is another agent: the code found for the old one does not stand.
            if (found.Length > 0 && code.Text.Trim() == found) code.Text = string.Empty;
            found = string.Empty;
            copied = false;
            addedTo = string.Empty;
            Update();
            _ = CheckAddedAsync();
        };
        app.SelectionChanged += (_, _) =>
        {
            // The name follows the application while it is still the other application's own.
            string shown = AgentApps.FirstOrDefault(a => a.Tag == App()).Name;
            if (AgentApps.Any(a => a.Name.Length > 0 && a.Name == name.Text.Trim()) || name.Text.Trim().Length == 0) name.Text = shown;
            format.SelectedIndex = App() == "claude" ? 1 : 0;
            addedTo = string.Empty;
            Update();
            _ = CheckAddedAsync();
        };
        // Omni already written into that application for this agent: step 1 shows done.
        async Task CheckAddedAsync()
        {
            if (!OneButton() || name.Text.Trim().Length == 0) return;
            // Read here, on the UI thread: from the pool the boxes' Text throws.
            (string app0, string agent0) = (App(), name.Text.Trim());
            WorkspaceAgentsRequest request = new("in_app", id, App: app0, Name: agent0, Exe: Environment.ProcessPath ?? "");
            try
            {
                string there = (await Task.Run(() => core.WorkspaceAgents(request))).AddedTo ?? "";
                if (App() == app0 && name.Text.Trim() == agent0 && addedTo.Length == 0) addedTo = there;
            }
            catch (Exception ex) { AppTrace.Write("workspace/agent-connect", $"in app check failed: {ex.Message}"); }
            Update();
        }
        add.Click += async (_, _) =>
        {
            error.IsOpen = false;
            // Read here, on the UI thread: from the pool the boxes' Text throws.
            string agent = name.Text.Trim();
            WorkspaceAgentsRequest request = new("add_to_app", id, App: App(), Name: agent, Folder: AgentFolder(agent), Exe: Environment.ProcessPath ?? "");
            add.IsEnabled = false;
            try
            {
                addedTo = (await Task.Run(() => core.WorkspaceAgents(request))).AddedTo ?? "";
            }
            catch (Exception ex)
            {
                AppTrace.Write("workspace/agent-connect", $"add to app FAILED: {ex}");
                error.Message = $"Не удалось вписать Omni в {AppName()}: {WorkspaceErrorText.ForCoreMessage(ex.Message)}";
                error.IsOpen = true;
            }
            Update();
            added.StartBringIntoView();
        };
        code.TextChanged += (_, _) => Update();
        where.SelectionChanged += (_, _) =>
        {
            // The code read here is not the other computer's agent.
            if (found.Length > 0 && code.Text.Trim() == found) code.Text = string.Empty;
            found = string.Empty;
            Update();
            _ = CheckAddedAsync();
        };
        format.SelectionChanged += (_, _) => { copied = false; Update(); };
        format.SelectedIndex = 1; // Claude, chosen first, reads JSON
        copy.Click += (_, _) =>
        {
            global::Windows.ApplicationModel.DataTransfer.DataPackage package = new();
            // Built again, not read from the box: its Text keeps line breaks as lone CRs, which a
            // config file does not take for line breaks.
            package.SetText(AgentSettings(Format(), name.Text, owner).ReplaceLineEndings("\r\n"));
            global::Windows.ApplicationModel.DataTransfer.Clipboard.SetContent(package);
            copied = true;
            Update();
            stepStarted.StartBringIntoView();
        };
        openFolder.Click += (_, _) =>
        {
            string folder = AgentFolder(name.Text);
            try
            {
                Directory.CreateDirectory(folder);
                System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo("explorer.exe", $"\"{folder}\"") { UseShellExecute = true });
            }
            catch (Exception ex) { error.Message = ex.Message; error.IsOpen = true; }
        };
        // The agent writes its code into its folder on its first run: read it from there.
        DispatcherTimer look = new() { Interval = TimeSpan.FromSeconds(2) };
        look.Tick += (_, _) =>
        {
            if (!Here() || name.Text.Trim().Length == 0 || code.Text.Trim().Length > 0) return;
            string file = Path.Combine(AgentFolder(name.Text), "agent-code.txt");
            try
            {
                if (File.Exists(file))
                {
                    found = File.ReadAllText(file).Trim();
                    code.Text = found;
                    // The box was collapsed until now: its TextChanged does not come, so the dialog
                    // follows here — and shows the step that just turned green.
                    Update();
                    stepStarted.StartBringIntoView();
                }
            }
            catch (IOException) { }
        };
        // The dialog closes only once the connection is asked for; a failure stays in it.
        dialog.PrimaryButtonClick += async (_, e) =>
        {
            var wait = e.GetDeferral();
            try
            {
                error.IsOpen = false;
                // The boxes are read here, on the UI thread: from the pool their Text throws.
                WorkspaceAgentsRequest connect = new("connect", id, Code: code.Text.Trim(), Name: name.Text.Trim(), MembersCommand: who.SelectedIndex == 1);
                WorkspaceAgents after = await Task.Run(() => core.WorkspaceAgents(connect));
                done(after);
                say($"{name.Text.Trim()} подключается: как только будет связь, он получит приглашение в workspace. Его состояние — в карточке ниже.", StatusKind.Done);
            }
            catch (Exception ex)
            {
                AppTrace.Write("workspace/agent-connect", $"connect FAILED: {ex}");
                error.Message = $"Не удалось подключить: {WorkspaceErrorText.ForCoreMessage(ex.Message)}";
                error.IsOpen = true;
                e.Cancel = true;
            }
            finally { wait.Complete(); }
        };
        look.Start();
        Update();
        _ = CheckAddedAsync();
        await dialog.ShowAsync();
        look.Stop();
    }

    /// The test channel's way to connect an agent with the code at hand (no dialog).
    internal Task<WorkspaceAgents> ConnectAgentForUiTestAsync(string code, string name)
    {
        if (bridge is null || groupId is null) throw new InvalidOperationException("no workspace open");
        RustBridge core = bridge; string id = groupId;
        return Task.Run(() => core.WorkspaceAgents(new("connect", id, Code: code, Name: name)));
    }
}
