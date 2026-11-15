using System;
using System.Linq;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Omni.Windows.App.Presentation;

namespace Omni.Windows.App.Features.Workspaces;

/// The projects of a workspace as rows of one kind (a code project and a folder of files alike): its
/// mark, its name, the one state that matters most, its last version. A row opens the project; the
/// work itself — saving, a proposal, the rules — is on the project's page.
public sealed partial class WorkspacesSectionControl
{
    // A code project's page, drawn in the projects' place (it is read with the rest of the workspace).
    private string? codeRepo;
    // The open files project's page redraws what it takes from the workspace (changes, states).
    private Action<WorkspaceScreen>? projectScreenChanged;

    private FrameworkElement ProjectRow(WorkspaceRepoState repo)
    {
        (string Chip, string Tone)? state = !repo.Here ? ("Не здесь", "Neutral") : repo.Files ? FilesState(repo) : CodeState(repo);
        string last = repo.Files
            ? repo.Main is null ? "Версий ещё нет." : (repo.Recent ?? new()).FirstOrDefault() is { } latest
                ? $"в.{repo.Version} · «{Shorten(latest.Subject, 70)}» · {latest.Author} · {MainWindowPresentation.FormatMessageTime(latest.AtUnixMs)}"
                : $"в.{repo.Version}"
            : repo.Main is { } main
                ? $"main · «{Shorten(main.Subject, 70)}» · {main.Author} · {MainWindowPresentation.FormatMessageTime(main.AtUnixMs)}"
                : "main ещё нет.";
        string? extra = repo.Files && repo.Waiting.Count > 0 ? $"предложений: {repo.Waiting.Count}"
            : !repo.Files && repo.Branches.Count > 0 ? $"веток: {repo.Branches.Count}" : null;

        Grid body = new() { ColumnSpacing = 12 };
        body.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        body.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        body.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        body.Children.Add(new FontIcon { Glyph = repo.Files ? "" : "", FontSize = 16, VerticalAlignment = VerticalAlignment.Center });
        StackPanel text = new() { Spacing = 2 };
        StackPanel name = new() { Orientation = Orientation.Horizontal, Spacing = 10 };
        name.Children.Add(new TextBlock { Text = repo.Repo, FontWeight = Microsoft.UI.Text.FontWeights.SemiBold, VerticalAlignment = VerticalAlignment.Center });
        if (state is { } shown)
        {
            Border chip = Chip(shown.Chip, shown.Tone);
            chip.VerticalAlignment = VerticalAlignment.Center;
            name.Children.Add(chip);
        }
        if (extra is not null) name.Children.Add(new TextBlock { Text = extra, FontSize = 12, Opacity = 0.7, VerticalAlignment = VerticalAlignment.Center });
        text.Children.Add(name);
        text.Children.Add(new TextBlock { Text = last, FontSize = 12, Opacity = 0.7, TextTrimming = TextTrimming.CharacterEllipsis });
        Grid.SetColumn(text, 1);
        body.Children.Add(text);
        FontIcon go = new() { Glyph = "", FontSize = 12, Opacity = 0.6, VerticalAlignment = VerticalAlignment.Center };
        Grid.SetColumn(go, 2);
        body.Children.Add(go);

        Button row = new() { Content = body, Style = (Style)Resources["WorkspaceProjectRowStyle"] };
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(row, state is { } named ? $"{repo.Repo} · {named.Chip}" : repo.Repo);
        row.Click += (_, _) =>
        {
            if (!repo.Here) _ = TakeRepoAsync(repo.Repo);
            else if (repo.Files) _ = ShowFilesAsync(repo.Repo);
            else OpenCode(repo.Repo);
        };

        // The one thing a row asks for at once, inside it before the chevron: laid over the row, not put
        // in it (a button inside the row's button would press both); the text keeps clear of it.
        Button? urgent = !repo.Files && repo.Here && repo.WorkPath is not null && repo.MainBehind > 0
            ? PlainButton("Забрать", false, () => _ = PullAsync(repo.Repo))
            : null;
        if (urgent is null) return row;
        text.Margin = new Thickness(0, 0, 96, 0);
        Grid line = new();
        line.Children.Add(row);
        urgent.HorizontalAlignment = HorizontalAlignment.Right;
        urgent.VerticalAlignment = VerticalAlignment.Center;
        urgent.Margin = new Thickness(0, 0, 40, 0);
        line.Children.Add(urgent);
        return line;
    }

    // The most pressing state of a folder of files: what stops work first, then what is mine, then calm.
    private static (string, string)? FilesState(WorkspaceRepoState repo)
    {
        WorkspaceFilesSync sync = repo.Sync ?? new WorkspaceFilesSync(new(), new(), 0, 0, null);
        if (!string.IsNullOrEmpty(sync.Problem)) return ("Не обновляется", "Critical");
        if (repo.Own is { State: "rejected" }) return ("Не принята", "Critical");
        if (repo.Conflicts is { Count: > 0 }) return ("Расхождение", "Caution");
        if (sync.WaitingChanged.Count > 0) return ("Пересечение", "Caution");
        if (repo.ChangedCount > 0) return ($"Есть изменения · {repo.ChangedCount}", "Caution");
        if (repo.Own is not null) return ("Ждёт принятия", "Accent");
        if (sync.WaitingOpen.Count > 0) return ("Ждёт файла", "Caution");
        if (repo.WorkPath is null) return ("Без папки", "Neutral");
        if (sync.ContentMissingBytes > 0) return ("Не всё здесь", "Caution");
        if (repo.Main is null) return null;
        return repo.FolderCurrent ? ("Актуально", "Success") : ("Обновляется", "Accent");
    }

    private static (string, string)? CodeState(WorkspaceRepoState repo)
    {
        if (repo.Forks.Count > 0) return ("main спорный", "Critical");
        if (repo.WorkPath is not null && repo.MainBehind > 0) return ($"main отстаёт · {repo.MainBehind}", "Caution");
        if (repo.Waiting.Count > 0) return ("Ждёт одобрения", "Accent");
        if (repo.WorkPath is null) return ("Без папки", "Neutral");
        return repo.Main is null ? null : ("Актуально", "Success");
    }

    /// A code project's page in the projects' place: its name (its buttons on the places' strip), then main,
    /// the rule, what waits and members' branches.
    internal void OpenCode(string repo)
    {
        if (!ProjectOpen && codeRepo is null) cardsOffset = WorkspaceCardsScroll.VerticalOffset;
        if (ProjectOpen) BackToCards();
        codeRepo = repo;
        signature = string.Empty;
        if (screen is not null) Draw(screen);
        WorkspaceCardsScroll.ChangeView(null, 0, null, disableAnimation: true);
    }

    // Its header over both areas; the state and the branches on the left, the main's history beside.
    private void DrawCodePage(WorkspaceScreen view, WorkspaceRepoState repo)
    {
        SetPlaceBar(CodeActions(view, repo));
        SetPlaceTitle(repo.Repo, null);
        WorkspaceCodePanel.Children.Add(TwoAreas(CodeBody(view, repo), CodeHistory(repo)));
    }
}
