using System;
using System.Linq;
using System.Threading.Tasks;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Omni.Windows.App.Presentation;

namespace Omni.Windows.App.Features.Workspaces;

/// What waits for a person — an agent's question to its owner, a branch asked to be checked — as one
/// card wherever it shows: on the agents' page and in the discussion of the work. The buttons send the same
/// core commands from both.
public sealed partial class WorkspacesSectionControl
{
    /// What a card needs from its page: the core, the workspace, the page's way to run a command (it
    /// says what happened and redraws), and the page's one-dialog-at-a-time gate.
    private sealed class CardHost(RustBridge core, string groupId, Func<WorkspaceAgentsRequest, string, Task<bool>> run)
    {
        public RustBridge Core { get; } = core;
        public string GroupId { get; } = groupId;
        public bool DialogOpen { get; set; }
        public Task<bool> Run(WorkspaceAgentsRequest request, string done) => run(request, done);
    }

    // An agent's question: into the shared version (what it brings, who checked it) or a yes/no.
    // On the agents' page: the way to the discussion of the work the card is about.
    private FrameworkElement WithTalkLink(FrameworkElement card, string? thread)
    {
        if (thread is null) return card;
        StackPanel both = new() { Spacing = 4 };
        both.Children.Add(card);
        HyperlinkButton open = new() { Content = "Открыть обсуждение работы", Padding = new Thickness(0) };
        open.Click += (_, _) => _ = ShowTalkAsync(thread);
        both.Children.Add(open);
        return both;
    }

    private FrameworkElement AskCard(CardHost host, WorkspaceAgentAsk ask)
    {
        StackPanel card = new() { Spacing = 8 };
        if (ask.Action != "main")
        {
            card.Children.Add(new TextBlock { Text = $"{ask.AgentName} спрашивает", Style = CardTitle, TextWrapping = TextWrapping.Wrap });
            card.Children.Add(Body(ask.Text));
            StackPanel yesNo = new() { Orientation = Orientation.Horizontal, Spacing = 8, Margin = new Thickness(0, 4, 0, 0) };
            yesNo.Children.Add(PlainButton("Да", true, () => _ = host.Run(new("answer", host.GroupId, Id: ask.Id, Allow: true), $"{ask.AgentName} получил ответ «да».")));
            yesNo.Children.Add(PlainButton("Нет…", false, () => _ = SendBackAsync(host, ask, "Ответить «нет»", "Почему нет (необязательно)", "Ответить «нет»", $"{ask.AgentName} получил ответ «нет».")));
            card.Children.Add(yesNo);
            return card;
        }
        card.Children.Add(new TextBlock { Text = $"{ask.AgentName} предлагает обновить общую версию «{ask.Repo}»", Style = CardTitle, TextWrapping = TextWrapping.Wrap });
        card.Children.Add(Caption("После «Применить» эти изменения станут общей версией проекта у всех участников."));
        if (ask.Text.Length > 0) card.Children.Add(Body(ask.Text));
        card.Children.Add(ask.AcceptedBy.Count > 0
            ? StateLine("Проверено", "Success", $"Проверил(а) {string.Join(", ", ask.AcceptedBy)} — тесты прошли.")
            : StateLine("Не проверено", "Caution", "Никто ещё не принял эти изменения."));
        if (ask.Commits is { Count: > 0 } commits)
        {
            card.Children.Add(SectionLabel("Что меняется"));
            foreach (string subject in commits) card.Children.Add(Body($"• {subject}"));
        }
        if (ask.Files is { Count: > 0 } files)
            card.Children.Add(Caption(string.Join(" · ", files.Select(FileChange))));
        if (ask.Commits is not { Count: > 0 })
            card.Children.Add(Caption("Изменения агента ещё идут сюда — «Применить» сработает, когда они придут."));
        StackPanel buttons = new() { Orientation = Orientation.Horizontal, Spacing = 8, Margin = new Thickness(0, 4, 0, 0) };
        buttons.Children.Add(PlainButton("Применить в общую версию", true, () => _ = host.Run(new("answer", host.GroupId, Id: ask.Id, Allow: true), $"Изменения {ask.AgentName} — общая версия «{ask.Repo}».")));
        buttons.Children.Add(PlainButton("Вернуть на доработку…", false, () => _ = SendBackAsync(host, ask, "Вернуть на доработку", "Что изменить", "Вернуть", $"{ask.AgentName} получил изменения обратно с вашим комментарием.")));
        buttons.Children.Add(PlainButton("Изменения…", false, () => _ = DiffAsync(host, $"Изменения {ask.AgentName} · {ask.Repo}", new("diff", host.GroupId, Id: ask.Id))));
        card.Children.Add(buttons);
        card.Children.Add(Caption($"ветка {ask.Branch} · {Short(ask.Oid ?? "")}"));
        return card;
    }

    // «Проверь изменения»: what the branch brings over our main, and the verdict on exactly its commit.
    private FrameworkElement ReviewCard(CardHost host, WorkspaceReviewAsk ask)
    {
        StackPanel card = new() { Spacing = 8 };
        card.Children.Add(new TextBlock { Text = $"{ask.AuthorName} просит проверить изменения в «{ask.Repo}»", Style = CardTitle, TextWrapping = TextWrapping.Wrap });
        card.Children.Add(Caption("«Принять» — вы посмотрели изменения и они годятся: после этого их можно предложить в общую версию. Вердикт относится к этому коммиту — после новых правок проверка нужна снова."));
        if (ask.Commits.Count > 0)
        {
            card.Children.Add(SectionLabel("Что меняется"));
            foreach (string subject in ask.Commits) card.Children.Add(Body($"• {subject}"));
        }
        if (ask.Files.Count > 0) card.Children.Add(Caption(string.Join(" · ", ask.Files.Select(FileChange))));
        if (!ask.Here) card.Children.Add(Caption("Изменения ещё идут сюда — принять можно, когда они придут."));
        StackPanel buttons = new() { Orientation = Orientation.Horizontal, Spacing = 8, Margin = new Thickness(0, 4, 0, 0) };
        Button accept = PlainButton("Принять", true, () => _ = host.Run(new("review", host.GroupId, Repo: ask.Repo, Branch: ask.Branch, Oid: ask.Oid, Accepted: true, TestsPassed: true), $"Изменения {ask.AuthorName} в «{ask.Repo}» приняты — {ask.AuthorName} узнает об этом."));
        accept.IsEnabled = ask.Here;
        buttons.Children.Add(accept);
        buttons.Children.Add(PlainButton("Вернуть на доработку…", false, () => _ = ReviewBackAsync(host, ask)));
        Button changes = PlainButton("Изменения…", false, () => _ = DiffAsync(host, $"Изменения {ask.AuthorName} · {ask.Repo}", new("diff", host.GroupId, Repo: ask.Repo, Oid: ask.Oid)));
        changes.IsEnabled = ask.Here;
        buttons.Children.Add(changes);
        card.Children.Add(buttons);
        card.Children.Add(Caption($"ветка {ask.Branch} · {Short(ask.Oid)} · {MainWindowPresentation.FormatMessageTime(ask.AtUnixMs)}"));
        return card;
    }

    private async Task ReviewBackAsync(CardHost host, WorkspaceReviewAsk ask)
    {
        if (host.DialogOpen) return;
        host.DialogOpen = true;
        try
        {
            TextBox reason = new() { Header = "Что изменить", AcceptsReturn = true, TextWrapping = TextWrapping.Wrap, MinHeight = 80, MaxLength = 4000 };
            ContentDialog dialog = new()
            {
                XamlRoot = XamlRoot, RequestedTheme = MainWindowPresentation.ActiveTheme,
                Title = "Вернуть на доработку", Content = reason,
                PrimaryButtonText = "Вернуть", CloseButtonText = "Отмена", DefaultButton = ContentDialogButton.Primary,
            };
            if (await dialog.ShowAsync() == ContentDialogResult.Primary)
                await host.Run(new("review", host.GroupId, Repo: ask.Repo, Branch: ask.Branch, Oid: ask.Oid, Accepted: false, Notes: reason.Text.Trim()), $"{ask.AuthorName} получил изменения обратно с вашим комментарием.");
        }
        finally { host.DialogOpen = false; }
    }

    private async Task SendBackAsync(CardHost host, WorkspaceAgentAsk ask, string title, string header, string primary, string done)
    {
        if (host.DialogOpen) return;
        host.DialogOpen = true;
        try
        {
            TextBox reason = new() { Header = header, AcceptsReturn = true, TextWrapping = TextWrapping.Wrap, MinHeight = 80, MaxLength = 4000 };
            ContentDialog dialog = new()
            {
                XamlRoot = XamlRoot, RequestedTheme = MainWindowPresentation.ActiveTheme,
                Title = title, Content = reason,
                PrimaryButtonText = primary, CloseButtonText = "Отмена", DefaultButton = ContentDialogButton.Primary,
            };
            if (await dialog.ShowAsync() == ContentDialogResult.Primary)
                await host.Run(new("answer", host.GroupId, Id: ask.Id, Allow: false, Reason: reason.Text.Trim()), done);
        }
        finally { host.DialogOpen = false; }
    }

    private async Task DiffAsync(CardHost host, string title, WorkspaceAgentsRequest request)
    {
        if (host.DialogOpen) return;
        host.DialogOpen = true;
        try
        {
            string text;
            try { text = (await Task.Run(() => host.Core.WorkspaceAgents(request))).Diff ?? ""; }
            catch (Exception ex) { text = WorkspaceErrorText.ForCoreMessage(ex.Message); }
            ScrollViewer view = new()
            {
                MaxHeight = 520, HorizontalScrollBarVisibility = ScrollBarVisibility.Auto, VerticalScrollBarVisibility = ScrollBarVisibility.Auto,
                Content = new TextBlock { Text = text, FontFamily = new Microsoft.UI.Xaml.Media.FontFamily("Consolas"), FontSize = 12, IsTextSelectionEnabled = true },
            };
            ContentDialog dialog = new()
            {
                XamlRoot = XamlRoot, RequestedTheme = MainWindowPresentation.ActiveTheme,
                Title = title, Content = view, CloseButtonText = "Закрыть",
            };
            dialog.Resources["ContentDialogMaxWidth"] = 900.0;
            await dialog.ShowAsync();
        }
        finally { host.DialogOpen = false; }
    }
}
