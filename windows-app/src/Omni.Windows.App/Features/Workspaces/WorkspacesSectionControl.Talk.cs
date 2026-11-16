using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Omni.Windows.App.Presentation;

namespace Omni.Windows.App.Features.Workspaces;

/// «Обсуждения» (docs/decisions/omni-workspace-git.md section 5): the conversation about a piece of
/// work — a task's, or what an agent was given — as people and agents write it: finished messages, not
/// an agent's reasoning. A person joins in; their line is data for the agents unless it names one they
/// may give work to, and the composer says which before it goes.
public sealed partial class WorkspacesSectionControl
{
    // The discussions have no project: this stands for them where the open place is named.
    private const string TalkPlace = "#talk";
    // Below this the list and the conversation take the page in turn.
    private const double TalkTwoPanesFrom = 760;

    /// A task's discussion: shown if it is there, else the page waits for the first line to open it
    /// (looking at a task opens nothing for the others to see).
    internal Task ShowTaskTalkAsync(string taskId, string topic) => ShowTalkAsync($"task-{taskId}", (topic, taskId));

    internal async Task ShowTalkAsync(string? thread = null, (string Topic, string Task)? start = null)
    {
        if (bridge is null || groupId is null) return;
        RustBridge core = bridge; string id = groupId;
        if (!ProjectOpen) cardsOffset = WorkspaceCardsScroll.VerticalOffset;
        int page = ++projectTicket;
        projectRepo = TalkPlace;
        projectScreenChanged = null;
        ShowSwitcher(Place.Talk);
        bool Live() => page == projectTicket;
        bool busy = false, toBottom = true;
        // Questions and checks of this work are the agents' page's cards, run through this page.
        CardHost cards = null!;
        // What waits for a person here comes with every answer: the agents' asks, checks and verdicts.
        WorkspaceAgents? place = null;
        string? current = thread;
        // A task's discussion not opened yet: its topic and task, until the first line opens it.
        (string Topic, string Task)? starting = start;
        List<WorkspaceTalkThread> threads = [];
        WorkspaceTalk? talk = null;
        WorkspaceTalkLine? replyTo = null;
        string listDrawn = "\0", talkDrawn = "\0";
        // The drawn messages by id: a quote goes to its message.
        Dictionary<string, FrameworkElement> bubbles = [];

        // «New» stands over the list it adds to, as in the chats.
        Button create = PlainButton("+ Новое", false, null);
        SetPlaceBar();

        void Say(string text, StatusKind kind = StatusKind.Done)
        {
            if (Live()) SetStatus(text, kind);
        }

        // The discussions on the left, the one open filling the rest: a chat, with a list of its own.
        Grid root = new();
        root.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(Features.Shared.ColumnWidth.PageList.Width) });
        root.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(0) });
        root.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        root.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });

        StackPanel list = new() { Spacing = 2 };
        Grid listHead = new() { Margin = new Thickness(0, 0, 0, 8) };
        TextBlock listTitle = SectionTitle("Обсуждения", first: true);
        listTitle.VerticalAlignment = VerticalAlignment.Center;
        listHead.Children.Add(listTitle);
        create.HorizontalAlignment = HorizontalAlignment.Right;
        listHead.Children.Add(create);
        Grid listPane = new() { Padding = new Thickness(16, 12, 12, 12) };
        listPane.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        listPane.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
        listPane.Children.Add(listHead);
        ScrollViewer listRows = new() { Content = list, VerticalScrollBarVisibility = ScrollBarVisibility.Auto };
        Grid.SetRow(listRows, 1);
        listPane.Children.Add(listRows);
        Border listScroll = new()
        {
            Child = listPane,
            BorderBrush = MainWindowPresentation.ResolveThemeBrush("OmniStrokeBrush", new Microsoft.UI.Xaml.Media.SolidColorBrush(Microsoft.UI.Colors.Gray)),
            BorderThickness = new Thickness(0, 0, 1, 0),
        };
        root.Children.Add(listScroll);
        root.Children.Add(new Features.Shared.RailSplitter { Source = Features.Shared.ColumnWidth.PageList });

        Grid conversation = new();
        conversation.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        conversation.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });
        conversation.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        Grid.SetColumn(conversation, 2);
        root.Children.Add(conversation);

        StackPanel header = new() { Spacing = 4, Margin = new Thickness(20, 12, 20, 12) };
        conversation.Children.Add(header);
        // As in the chats: the list stands 8 off the edge and the bubbles 20 inside it.
        StackPanel lines = new() { Padding = new Thickness(16, 0, 20, 4) };
        ScrollViewer linesScroll = new() { Content = lines, Margin = new Thickness(0, 0, 8, 0), VerticalScrollBarVisibility = ScrollBarVisibility.Auto };
        Grid.SetRow(linesScroll, 1);
        conversation.Children.Add(linesScroll);

        // The chats' composer: `@` names whom the line is for; over the field, what it will be for them.
        Features.Shared.MessageComposer composer = new() { ShowAttach = false };
        Grid.SetRow(composer, 2);
        conversation.Children.Add(composer);
        TextBlock hint = Caption("");
        hint.Margin = new Thickness(0, 0, 0, 6);
        hint.Visibility = Visibility.Collapsed;
        composer.Above = hint;
        TextBox text = composer.Input;
        text.MaxLength = 12 * 1024;

        WorkspaceProjectHost.Children.Clear();
        WorkspaceProjectHost.Children.Add(root);
        WorkspaceCardsScroll.Visibility = Visibility.Collapsed;
        WorkspaceProjectHost.Visibility = Visibility.Visible;

        bool Shown() => current is not null;
        bool? narrow = null;
        // A narrow page shows one of the two: the list, or the discussion with the way back to it.
        void Arrange(double width)
        {
            narrow = width < TalkTwoPanesFrom;
            bool talkOnly = narrow == true && Shown();
            bool listOnly = narrow == true && !Shown();
            root.ColumnDefinitions[0].Width = talkOnly ? new GridLength(0) : listOnly ? new GridLength(1, GridUnitType.Star) : new GridLength(Features.Shared.ColumnWidth.PageList.Width);
            root.ColumnDefinitions[2].Width = listOnly ? new GridLength(0) : new GridLength(1, GridUnitType.Star);
            listScroll.Visibility = talkOnly ? Visibility.Collapsed : Visibility.Visible;
            conversation.Visibility = listOnly ? Visibility.Collapsed : Visibility.Visible;
        }
        void FollowWidth()
        {
            if (!Live()) { Features.Shared.ColumnWidth.PageList.Changed -= FollowWidth; return; }
            Arrange(root.ActualWidth > 0 ? root.ActualWidth : TalkTwoPanesFrom);
        }
        Features.Shared.ColumnWidth.PageList.Changed += FollowWidth;
        // A wider conversation has wider messages, whichever edge moved.
        linesScroll.SizeChanged += (_, e) =>
        {
            if (Math.Abs(e.NewSize.Width - e.PreviousSize.Width) > 40) DispatcherQueue.TryEnqueue(() => { if (Live()) DrawTalk(); });
        };
        root.SizeChanged += (_, e) =>
        {
            bool now = e.NewSize.Width < TalkTwoPanesFrom;
            if (now != narrow) Arrange(e.NewSize.Width);
            if (now != narrow) DispatcherQueue.TryEnqueue(() => { if (Live()) DrawTalk(); });
        };

        string ListSignature() => string.Join(";", threads.Select(t => $"{t.Id},{t.LastAtUnixMs},{t.Lines},{t.Topic}")) + "|" + current;
        string TalkSignature() => (talk is null ? $"-{current}{starting is null}" : $"{talk.Thread.Id},{talk.Thread.Topic},{talk.Lines.Count},{talk.Lines.LastOrDefault()?.Id},{string.Join(",", talk.Lines.Where(l => l.SeenBy is { Count: > 0 }).Select(l => $"{l.Id}:{string.Join("+", l.SeenBy!)}"))},{string.Join(",", talk.Lines.Where(l => l.Reactions is { Count: > 0 }).Select(l => l.Id + string.Concat(l.Reactions!.Select(r => $"{r.Emoji}{r.Count}{r.Mine}"))))},{string.Join(",", talk.People.Select(p => $"{p.Account}:{p.Commanded}"))},{starting is null}")
            + "|" + CardsSignature();
        string CardsSignature() => place is null || current is null ? "" : string.Join(";",
            place.Asks.Where(a => a.Thread == current).Select(a => $"{a.Id},{a.Answer},{a.CanAnswer},{a.Commits?.Count}")
                .Concat((place.ReviewAsks ?? []).Where(r => r.Thread == current).Select(r => $"{r.Oid},{r.Here},{r.CanReview}"))
                .Concat(place.Reviews.Where(r => r.Thread == current).Select(r => $"{r.Oid},{r.ReviewerName},{r.Accepted},{r.AtUnixMs}")));

        void Take(WorkspaceAgents view)
        {
            if (!Live()) return;
            threads = view.Threads ?? [];
            place = view;
            if (view.Opened is { } opened) current = opened;
            // Someone else opened the task's discussion meanwhile: it is the real one now.
            if (starting is not null && threads.Any(t => t.Id == current)) starting = null;
            if (view.Talk is { } read && read.Thread.Id == current) talk = read;
            if (ListSignature() != listDrawn) DrawList();
            if (TalkSignature() != talkDrawn) DrawTalk();
        }

        async Task<bool> Run(WorkspaceAgentsRequest request)
        {
            try
            {
                Take(await Task.Run(() => core.WorkspaceAgents(request)));
                return true;
            }
            catch (Exception ex)
            {
                Say(WorkspaceErrorText.ForCoreMessage(ex.Message), StatusKind.Failed);
                return false;
            }
        }

        async Task Reload()
        {
            if (busy) return;
            busy = true;
            try
            {
                if (current is { } shown) await Run(new("talk_read", id, Thread: shown));
                else
                {
                    await Run(new("talk_list", id));
                    if (current is null && threads.FirstOrDefault() is { } latest && narrow != true)
                    {
                        current = latest.Id;
                        await Run(new("talk_read", id, Thread: latest.Id));
                    }
                }
            }
            finally { busy = false; }
        }

        async Task Select(string? chosen)
        {
            if (chosen == current && chosen is not null) return;
            current = chosen;
            starting = null;
            talk = null;
            SetReply(null);
            toBottom = true;
            Arrange(root.ActualWidth > 0 ? root.ActualWidth : TalkTwoPanesFrom);
            DrawList();
            DrawTalk();
            if (chosen is null) return;
            busy = true;
            try { await Run(new("talk_read", id, Thread: chosen)); }
            finally { busy = false; }
        }

        void DrawList()
        {
            listDrawn = ListSignature();
            list.Children.Clear();
            listTitle.Text = threads.Count > 0 ? $"Обсуждения · {threads.Count}" : "Обсуждения";
            if (threads.Count == 0 && starting is null)
            {
                list.Children.Add(Caption("Обсуждение открывается, когда вы даёте работу агенту или пишете в обсуждение задачи. Здесь видно, что люди и агенты пишут о работе, и можно вступить."));
                list.Children.Add(Caption("Обсуждения, которые шли до вашего подключения к workspace, сюда не приходят."));
                return;
            }
            if (starting is { } s && current is not null)
                list.Children.Add(ThreadRow(current, s.Topic, "Ещё не начато — первое сообщение его откроет.", "", true));
            foreach (WorkspaceTalkThread t in threads)
            {
                string topic = t.Topic.Length > 0 ? t.Topic : "Обсуждение";
                string last = t.LastAuthor is { Length: > 0 } who ? $"{who}: {t.LastText}" : "Пока без сообщений";
                string about = MainWindowPresentation.FormatMessageTime(t.LastAtUnixMs > 0 ? t.LastAtUnixMs : t.AtUnixMs)
                    + (t.Agents.Count > 0 ? " · " + string.Join(", ", t.Agents) : "");
                list.Children.Add(ThreadRow(t.Id, topic, last, about, t.Id == current));
            }
        }

        FrameworkElement ThreadRow(string thread, string topic, string last, string about, bool on)
        {
            StackPanel row = new() { Spacing = 2 };
            row.Children.Add(new TextBlock { Text = topic, FontWeight = Microsoft.UI.Text.FontWeights.SemiBold, TextTrimming = TextTrimming.CharacterEllipsis });
            TextBlock said = new() { Text = last.ReplaceLineEndings(" "), TextWrapping = TextWrapping.Wrap, MaxLines = 2, TextTrimming = TextTrimming.CharacterEllipsis };
            row.Children.Add(said);
            if (about.Length > 0) row.Children.Add(Caption(about));
            Button door = new() { Content = row, Style = (Style)Resources[on ? "WorkspaceTalkRowOnStyle" : "WorkspaceTalkRowStyle"] };
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(door, topic);
            door.Click += (_, _) => _ = Select(thread);
            return door;
        }

        void DrawTalk()
        {
            talkDrawn = TalkSignature();
            header.Children.Clear();
            lines.Children.Clear();
            composer.Visibility = Shown() ? Visibility.Visible : Visibility.Collapsed;
            if (!Shown())
            {
                if (threads.Count > 0) lines.Children.Add(Caption("Выберите обсуждение слева."));
                return;
            }
            if (narrow == true)
            {
                Button back = PlainButton("Все обсуждения", false, () => _ = Select(null));
                back.Margin = new Thickness(0, 0, 0, 4);
                header.Children.Add(back);
            }
            WorkspaceTalkThread? record = talk?.Thread;
            string topic = starting?.Topic ?? (record?.Topic is { Length: > 0 } named ? named : "Обсуждение");
            header.Children.Add(new TextBlock { Text = topic, Style = CardTitle, TextWrapping = TextWrapping.Wrap });
            if (starting is not null)
                header.Children.Add(Caption("Этого обсуждения ещё нет: первое сообщение его откроет, его увидят все участники workspace."));
            else if (record is not null && record.Topic.Length == 0)
                header.Children.Add(Caption("Начало обсуждения сюда не пришло: оно было до вашего подключения к workspace или ещё в пути."));
            else if (record is not null)
                header.Children.Add(Caption($"Начал(а) {record.OpenedByName} · {MainWindowPresentation.FormatMessageTime(record.AtUnixMs)}"
                    + (record.Agents.Count > 0 ? " · агенты: " + string.Join(", ", record.Agents) : "")));

            if (talk is null)
            {
                lines.Children.Add(Caption(starting is null ? "Читаю обсуждение…" : ""));
            }
            else
            {
                Dictionary<string, WorkspaceTalkLine> byId = talk.Lines.ToDictionary(l => l.Id);
                bubbles.Clear();
                List<(long At, FrameworkElement Shown)> timeline = talk.Lines
                    .Select((l, i) => (l.AtUnixMs, Bubble(l, i > 0 ? talk.Lines[i - 1] : null, i + 1 < talk.Lines.Count ? talk.Lines[i + 1] : null, byId)))
                    .ToList();
                timeline.AddRange(CardsHere());
                if (timeline.Count == 0 && starting is null) lines.Children.Add(Caption("Пока ни одного сообщения."));
                foreach ((_, FrameworkElement shown) in timeline.OrderBy(t => t.At)) lines.Children.Add(shown);
            }
            DrawPeople();
            UpdateHint();
            if (toBottom || linesScroll.VerticalOffset >= linesScroll.ScrollableHeight - 24)
            {
                toBottom = false;
                linesScroll.UpdateLayout();
                linesScroll.ChangeView(null, linesScroll.ScrollableHeight, null, true);
            }
        }

        // The work's questions and checks where they came: what is ours to decide as the agents' page's
        // card (the same buttons), what waits for someone else or is decided as one line of Omni.
        IEnumerable<(long, FrameworkElement)> CardsHere()
        {
            if (place is null || current is null) yield break;
            foreach (WorkspaceAgentAsk ask in place.Asks.Where(a => a.Thread == current))
            {
                if (ask.CanAnswer) yield return (ask.AtUnixMs, CardInTalk(AskCard(cards, ask)));
                else if (ask.Answer is null)
                    yield return (ask.AtUnixMs, OmniLine("Ждёт ответа", "Caution", ask.Action == "main"
                        ? $"{ask.AgentName} просит хозяина применить изменения в общую версию «{ask.Repo}»"
                        : $"{ask.AgentName} спрашивает хозяина: «{ask.Text}»", ask.AtUnixMs));
                else
                {
                    (string chip, string tone) = Answered(ask);
                    string why = string.IsNullOrEmpty(ask.Reason) ? "" : $" — «{ask.Reason}»";
                    long at = ask.AnsweredUnixMs > 0 ? ask.AnsweredUnixMs : ask.AtUnixMs;
                    yield return (at, OmniLine(chip, tone, AnsweredWhat(ask) + why, at));
                }
            }
            foreach (WorkspaceReviewAsk ask in (place.ReviewAsks ?? []).Where(r => r.Thread == current))
                yield return (ask.AtUnixMs, ask.CanReview
                    ? CardInTalk(ReviewCard(cards, ask))
                    : OmniLine("Ждёт проверки", "Caution", $"{ask.AuthorName} просит проверить изменения в «{ask.Repo}» · {ask.Branch}", ask.AtUnixMs));
            foreach (WorkspaceAgentReview review in place.Reviews.Where(r => r.Thread == current))
            {
                (string chip, string tone) = review.Accepted && review.TestsPassed ? ("Принято", "Success") : review.Accepted ? ("Тесты не прошли", "Caution") : ("Нужны правки", "Caution");
                string notes = string.IsNullOrEmpty(review.Notes) ? "" : $" — «{review.Notes}»";
                yield return (review.AtUnixMs, OmniLine(chip, tone, $"{review.ReviewerName} проверил(а) изменения в «{review.Repo}» · {review.Branch}{notes}", review.AtUnixMs));
            }
        }

        FrameworkElement CardInTalk(FrameworkElement card)
        {
            FrameworkElement framed = Card(card);
            framed.Margin = new Thickness(0, 0, 0, 10);
            framed.MaxWidth = BubbleWidth();
            framed.HorizontalAlignment = HorizontalAlignment.Left;
            return framed;
        }

        // A thing Omni says happened in the discussion: its state chip, what, and when.
        FrameworkElement OmniLine(string chip, string tone, string what, long at)
        {
            StackPanel line = new() { Spacing = 2, Margin = new Thickness(4, 2, 0, 12), MaxWidth = BubbleWidth(), HorizontalAlignment = HorizontalAlignment.Left };
            line.Children.Add(StateLine(chip, tone, what));
            line.Children.Add(Caption(MainWindowPresentation.FormatMessageTime(at)));
            return line;
        }

        // The chats' rule: a share of the conversation's width.
        double BubbleWidth() => MainWindowPresentation.CalculateTranscriptBubbleMaxWidth(linesScroll.ActualWidth, MainWindowPresentation.TranscriptRowChromeWidth);

        // The one message template; here its author's look, whether it is an agent's, and what it answers.
        FrameworkElement Bubble(WorkspaceTalkLine line, WorkspaceTalkLine? before, WorkspaceTalkLine? after, Dictionary<string, WorkspaceTalkLine> byId)
        {
            static bool SameRun(WorkspaceTalkLine a, WorkspaceTalkLine b) => a.Mine == b.Mine && a.AuthorName == b.AuthorName;
            static DateTime Day(WorkspaceTalkLine l) => DateTimeOffset.FromUnixTimeMilliseconds(l.AtUnixMs).ToLocalTime().Date;
            bool newDay = before is null || Day(before) != Day(line);
            string got = line.SeenBy is { Count: > 0 } seen ? $" · ✓ получил{(seen.Count > 1 ? "и" : "")}: {string.Join(", ", seen)}" : "";
            Features.Chats.ChatTranscriptPeer peer = new();
            peer.Set(line.AuthorName.Length > 0 ? line.AuthorName[..1].ToUpperInvariant() : "?", null, line.AuthorName);
            Features.Shared.MessageRow row = new(line.Id)
            {
                IsOutgoing = line.Mine,
                Body = line.Text,
                TimeText = MainWindowPresentation.FormatBubbleTime(line.AtUnixMs) + got,
                SenderName = line.AuthorName,
                ShowSender = newDay || !SameRun(before!, line),
                IsRunTail = after is null || Day(after) != Day(line) || !SameRun(line, after),
                NoteText = line.Agent ? (line.OwnerName is { Length: > 0 } owner ? $"агент {owner}" : "агент") : "",
                ChipText = line.CommandTo.Count > 0 ? $"поручение: {string.Join(", ", line.CommandTo)}" : "",
                Peer = peer,
            };
            row.SetDayHeader(newDay ? MainWindowPresentation.FormatDayHeader(line.AtUnixMs, DateTime.Today) : null);
            row.UpdateBubbleMaxWidth(BubbleWidth());
            row.ApplyReactions(line.Reactions);
            if (line.ReplyTo is { } answered && byId.TryGetValue(answered, out WorkspaceTalkLine? first))
                row.SetQuote(answered, first.Mine ? "Вы" : first.AuthorName, first.Text);
            Features.Shared.MessageBubble bubble = new() { Row = row };
            bubble.ReplyRequested += (_, _) => SetReply(line);
            bubble.ReactionChosen += async (_, emoji) =>
            {
                string? thread = current;
                if (thread is null) return;
                await Run(new("talk_react", id, Thread: thread, Line: line.Id, Emoji: emoji));
            };
            bubble.DeleteOwnOnly = true;
            bubble.DeleteRequested += async (_, _) =>
            {
                string? thread = current;
                if (thread is null || XamlRoot is null) return;
                if (await Features.Shared.MessageActions.AskDeleteAsync(XamlRoot, 1, "У всех", everyoneOnly: true) is null) return;
                await Run(new("talk_delete", id, Thread: thread, Line: line.Id));
            };
            bubble.ForwardRequested += async (_, shownRow) =>
            {
                if (bridge is { } here && XamlRoot is not null)
                    await Features.Shared.MessageActions.ForwardAsync(XamlRoot, here, [shownRow]);
            };
            bubble.QuoteOpened += (_, opened) =>
            {
                if (opened.QuotedMessageId is { } target && bubbles.TryGetValue(target, out FrameworkElement? shown))
                    shown.StartBringIntoView(new BringIntoViewOptions { AnimationDesired = true, VerticalAlignmentRatio = 0.3 });
            };
            bubbles[line.Id] = bubble;
            return bubble;
        }

        // Who a line here can name; an agent's note says whose it is.
        void DrawPeople() => composer.SetMentionCandidates((talk?.People ?? [])
            .Select(p => new Features.Shared.MentionCandidate(p.Account, p.Name, p.Agent ? (p.OwnerName.Length > 0 ? $"агент {p.OwnerName}" : "агент") : ""))
            .ToList());

        // What the line will be, seen before it goes; a line naming nobody is simply for everyone.
        void UpdateHint()
        {
            string written = text.Text ?? "";
            List<WorkspaceTalkPerson> named = (talk?.People ?? []).Where(p => written.Contains("@" + p.Name, StringComparison.OrdinalIgnoreCase)).ToList();
            List<string> parts = [];
            string work = string.Join(", ", named.Where(p => p.Agent && p.Commanded).Select(p => p.Name));
            string data = string.Join(", ", named.Where(p => p.Agent && !p.Commanded).Select(p => p.Name));
            if (work.Length > 0) parts.Add($"{work}: поручение.");
            if (data.Length > 0) parts.Add($"{data}: только сведения, поручения даёт хозяин агента.");
            hint.Text = string.Join(" ", parts);
            hint.Visibility = parts.Count > 0 ? Visibility.Visible : Visibility.Collapsed;
        }

        void SetReply(WorkspaceTalkLine? line)
        {
            replyTo = line;
            if (line is null)
            {
                composer.ClearReply();
                return;
            }
            composer.ShowReply(line.Mine ? "Вы" : line.AuthorName, line.Text);
            // Answering an agent addresses it: as work when it is ours to give, else as a mention.
            if (line.Agent && !line.Mine)
            {
                string note = talk?.People.FirstOrDefault(p => p.Account == line.AuthorAccount) is { Agent: true } person && person.OwnerName.Length > 0
                    ? $"агент {person.OwnerName}"
                    : "агент";
                composer.AddressTo(new Features.Shared.MentionCandidate(line.AuthorAccount, line.AuthorName, note));
            }
        }

        async Task Send()
        {
            // Read here, on the UI thread: from the pool the box's Text throws.
            string body = text.Text.Trim();
            if (body.Length == 0 || busy || current is null) return;
            List<string> named = composer.TakeMentions();
            string? answering = replyTo?.Id;
            busy = true;
            composer.SendButton.IsEnabled = false;
            try
            {
                string thread = current;
                if (starting is { } s)
                {
                    WorkspaceAgents opened = await Task.Run(() => core.WorkspaceAgents(new("talk_open", id, Topic: s.Topic, Task: s.Task)));
                    if (!Live()) return;
                    thread = opened.Opened ?? thread;
                    current = thread;
                    starting = null;
                }
                WorkspaceAgents after = await Task.Run(() => core.WorkspaceAgents(new("talk_send", id, Thread: thread, Text: body, ReplyTo: answering, Mentions: named)));
                if (!Live()) return;
                text.Text = string.Empty;
                SetReply(null);
                toBottom = true;
                Take(after);
            }
            catch (Exception ex)
            {
                AppTrace.Write("workspace/talk", $"send FAILED: {ex}");
                Say(WorkspaceErrorText.ForCoreMessage(ex.Message), StatusKind.Failed);
            }
            finally
            {
                busy = false;
                composer.SendButton.IsEnabled = true;
            }
        }

        async Task CreateAsync()
        {
            if (cards.DialogOpen) return;
            cards.DialogOpen = true;
            try
            {
                List<WorkspaceTask> open = [];
                try { open = (await Task.Run(() => core.WorkspaceFiles(new("tasks", id, TasksRepo)))).Tasks?.Tasks.Where(t => t.Status != "done").OrderBy(t => t.Number).ToList() ?? []; }
                catch (Exception) { /* without the board a discussion is still started by its topic */ }
                StackPanel form = new() { Spacing = 10, MinWidth = 440 };
                form.Children.Add(Body("Обсуждение видят все участники workspace и их агенты. Агент берёт его в работу, только когда ему пишет поручение тот, кто может его дать."));
                TextBox topic = new() { Header = "О чём", PlaceholderText = "Например: размеры крышки", MaxLength = 200 };
                ComboBox task = new() { Header = "Задача", HorizontalAlignment = HorizontalAlignment.Stretch };
                task.Items.Add(new ComboBoxItem { Content = "Без задачи", Tag = "" });
                foreach (WorkspaceTask t in open) task.Items.Add(new ComboBoxItem { Content = $"#{t.Number} {t.Title}", Tag = t.Id });
                task.SelectedIndex = 0;
                InfoBar error = new() { IsOpen = false, IsClosable = false, Severity = InfoBarSeverity.Error };
                form.Children.Add(topic);
                form.Children.Add(task);
                form.Children.Add(error);
                ContentDialog dialog = new()
                {
                    XamlRoot = XamlRoot, RequestedTheme = MainWindowPresentation.ActiveTheme,
                    Title = "Новое обсуждение", Content = form,
                    PrimaryButtonText = "Начать", CloseButtonText = "Отмена", DefaultButton = ContentDialogButton.Primary,
                    IsPrimaryButtonEnabled = false,
                };
                string? TaskId() => (task.SelectedItem as ComboBoxItem)?.Tag is string { Length: > 0 } t ? t : null;
                void Update() => dialog.IsPrimaryButtonEnabled = topic.Text.Trim().Length > 0 || TaskId() is not null;
                topic.TextChanged += (_, _) => Update();
                task.SelectionChanged += (_, _) =>
                {
                    if (topic.Text.Trim().Length == 0 && task.SelectedItem is ComboBoxItem { Tag: string { Length: > 0 } } item) topic.Text = (string)item.Content;
                    Update();
                };
                dialog.PrimaryButtonClick += async (_, e) =>
                {
                    var wait = e.GetDeferral();
                    try
                    {
                        WorkspaceAgentsRequest request = new("talk_open", id, Topic: topic.Text.Trim(), Task: TaskId());
                        WorkspaceAgents opened = await Task.Run(() => core.WorkspaceAgents(request));
                        starting = null;
                        talk = null;
                        toBottom = true;
                        Take(opened);
                        Arrange(root.ActualWidth > 0 ? root.ActualWidth : TalkTwoPanesFrom);
                        DrawList();
                        DrawTalk();
                        text.Focus(FocusState.Programmatic);
                    }
                    catch (Exception ex)
                    {
                        error.Message = WorkspaceErrorText.ForCoreMessage(ex.Message);
                        error.IsOpen = true;
                        e.Cancel = true;
                    }
                    finally { wait.Complete(); }
                };
                await dialog.ShowAsync();
            }
            finally { cards.DialogOpen = false; }
        }

        // A card's answer goes to the core as on the agents' page; then the discussion is read again.
        cards = new(core, id, async (request, done) =>
        {
            try
            {
                await Task.Run(() => core.WorkspaceAgents(request));
                Say(done);
            }
            catch (Exception ex)
            {
                Say(WorkspaceErrorText.ForCoreMessage(ex.Message), StatusKind.Failed);
                return false;
            }
            await Reload();
            return true;
        });
        composer.TextEdited += (_, _) => UpdateHint();
        composer.ReplyDismissed += (_, _) => SetReply(null);
        composer.SendRequested += (_, _) => _ = Send();
        create.Click += (_, _) => _ = CreateAsync();

        Arrange(TalkTwoPanesFrom);
        DrawList();
        DrawTalk();
        await Reload();
        DispatcherTimer timer = new() { Interval = TimeSpan.FromSeconds(3) };
        timer.Tick += async (_, _) =>
        {
            if (!Live()) { timer.Stop(); return; }
            if (!cards.DialogOpen) await Reload();
        };
        timer.Start();
    }
}
