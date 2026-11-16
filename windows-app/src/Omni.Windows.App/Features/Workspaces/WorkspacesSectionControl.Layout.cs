using System;
using System.Collections.Generic;
using System.Linq;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Omni.Windows.App.Presentation;

namespace Omni.Windows.App.Features.Workspaces;

/// How the section's pages are laid out, in one place: every page starts at one left edge and takes the
/// window to its right edge: two areas (the work on the left, what goes with it on the right), rows and
/// cards in columns (Shared.PageColumns), the board across them. The places' strip above is the place's header: its actions sit at that edge. A project's page has its own header: «← К
/// проектам», its name, its actions. Two areas fold into one column on a narrow window.
public sealed partial class WorkspacesSectionControl
{
    // A form that is read across, however wide the window; rows and cards take PageColumns instead.
    private const double ColumnMax = 900;
    private const double AreaGap = 24;
    // Below this the side area goes under the main one: both would be squeezed side by side.
    private const double TwoAreasFrom = 1100;
    private const double Gutter = 24;
    // A files project: one pane at a time (the list or the chosen file) below this.
    private const double FilesTwoPanesFrom = 900;
    // What a page's vertical scroll bar keeps at its right: the places' strip ends where the page does.
    private const double ScrollBarRoom = 14;
    // A commit's line with its hash: the hash stays near the text it names.
    private const double CommitLineMax = 680;

    // A page without a scroll of its own keeps the same right edge as one with it.
    private static readonly Thickness UnscrolledPadding = new(Gutter, Gutter - 8, Gutter + ScrollBarRoom, 16);

    /// The place's own actions and filters, on their row under the places' strip. A project's name goes
    /// on the strip, after the places (SetPlaceTitle); any other place clears it.
    private void SetPlaceBar(params UIElement[] actions)
    {
        WorkspacePlaceTitle.Children.Clear();
        WorkspacePlaceActions.Children.Clear();
        foreach (UIElement action in actions)
        {
            // Filters and buttons of different heights on one middle line.
            if (action is FrameworkElement element) element.VerticalAlignment = VerticalAlignment.Center;
            WorkspacePlaceActions.Children.Add(action);
        }
        WorkspacePlaceActions.Visibility = actions.Length > 0 ? Visibility.Visible : Visibility.Collapsed;
    }

    /// `content` at most `max` wide, from the left edge (a capped star column leaves the rest empty).
    private static Grid Bounded(UIElement content, double max)
    {
        Grid frame = new();
        frame.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star), MaxWidth = max });
        frame.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        frame.Children.Add(content);
        return frame;
    }

    /// The work on the left, what goes with it on the right, in the same proportion on any window; under
    /// it when the page is narrower than both need.
    private static Grid TwoAreas(FrameworkElement main, FrameworkElement side)
    {
        Grid areas = new() { RowSpacing = 24 };
        areas.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(3, GridUnitType.Star) });
        areas.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(2, GridUnitType.Star) });
        areas.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        areas.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        areas.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        main.VerticalAlignment = side.VerticalAlignment = VerticalAlignment.Top;
        areas.Children.Add(main);
        areas.Children.Add(side);
        bool? wide = null;
        void Arrange(double width)
        {
            bool now = width >= TwoAreasFrom;
            if (wide == now) return;
            wide = now;
            Grid.SetRow(side, now ? 0 : 1);
            Grid.SetColumn(side, now ? 1 : 0);
            areas.ColumnDefinitions[1].Width = now ? new GridLength(2, GridUnitType.Star) : new GridLength(0);
            areas.ColumnSpacing = now ? AreaGap : 0;
        }
        Arrange(TwoAreasFrom);
        areas.SizeChanged += (_, e) => Arrange(e.NewSize.Width);
        return areas;
    }

    /// A project's name, its own tabs beside it (a files project), on the places' strip after a hairline:
    /// the way back to the list is «Проекты» itself, the project's actions are at the strip's right.
    private void SetPlaceTitle(string name, FrameworkElement? beside)
    {
        WorkspacePlaceTitle.Children.Clear();
        WorkspacePlaceTitle.Children.Add(new Border { Style = (Style)Resources["WorkspacePlaceHairlineStyle"] });
        WorkspacePlaceTitle.Children.Add(new TextBlock
        {
            Text = name,
            Style = CardTitle,
            VerticalAlignment = VerticalAlignment.Center,
            TextTrimming = TextTrimming.CharacterEllipsis,
            MaxWidth = 320,
        });
        if (beside is not null)
        {
            beside.VerticalAlignment = VerticalAlignment.Center;
            beside.Margin = new Thickness(0);
            WorkspacePlaceTitle.Children.Add(beside);
        }
    }

    /// A section of a page: the settings' group title (accent, semibold) — the app's one language.
    private static TextBlock SectionTitle(string text, bool first = false)
    {
        TextBlock title = new() { Text = text, Style = (Style)Application.Current.Resources["SettingGroupTitleTextStyle"] };
        if (first) title.Margin = new Thickness(0, 0, 0, 2);
        return title;
    }

    /// «Что нового»: what others did here lately, each line leading to its project, the tasks or the
    /// agents; grouped by day.
    private FrameworkElement NewsArea(WorkspaceScreen view)
    {
        StackPanel area = new() { Spacing = 2 };
        area.Children.Add(SectionTitle("Что нового", first: true));
        List<WorkspaceNews> news = view.News ?? [];
        if (news.Count == 0)
        {
            area.Children.Add(Caption("Здесь появится, что участники и агенты делают в проектах, задачах и ветках."));
            return area;
        }
        string? day = null;
        foreach (WorkspaceNews line in news)
        {
            string lineDay = DayOf(line.AtUnixMs);
            if (lineDay != day)
            {
                day = lineDay;
                TextBlock label = Caption(lineDay);
                label.Margin = new Thickness(0, area.Children.Count > 1 ? 10 : 4, 0, 2);
                area.Children.Add(label);
            }
            area.Children.Add(NewsRow(view, line));
        }
        return area;
    }

    private FrameworkElement NewsRow(WorkspaceScreen view, WorkspaceNews line)
    {
        StackPanel text = new() { Spacing = 1 };
        text.Children.Add(new TextBlock { Text = line.Text, TextWrapping = TextWrapping.Wrap, MaxLines = 3, TextTrimming = TextTrimming.CharacterEllipsis });
        text.Children.Add(Caption(MainWindowPresentation.FormatMessageTime(line.AtUnixMs)));
        Action? open = line.About switch
        {
            AgentsPlace => () => _ = ShowAgentsAsync(),
            { } talk when talk.StartsWith(TalkPlace + ":", StringComparison.Ordinal) => () => _ = ShowTalkAsync(talk[(TalkPlace.Length + 1)..]),
            TasksRepo => () => _ = ShowTasksAsync(),
            { Length: > 0 } repo when view.Repos.FirstOrDefault(r => r.Repo == repo) is { Here: true } state
                => state.Files ? () => _ = ShowFilesAsync(repo) : () => OpenCode(repo),
            _ => null,
        };
        if (open is null)
        {
            text.Margin = new Thickness(0, 6, 0, 6);
            return text;
        }
        Button row = new() { Content = text, Style = (Style)Resources["WorkspaceNewsRowStyle"] };
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(row, line.Text);
        row.Click += (_, _) => open();
        return row;
    }

    // «Сегодня», «Вчера», «2 ноября».
    private static string DayOf(long unixMs)
    {
        DateTime at = DateTimeOffset.FromUnixTimeMilliseconds(unixMs).LocalDateTime.Date;
        DateTime today = DateTime.Now.Date;
        if (at == today) return "Сегодня";
        if (at == today.AddDays(-1)) return "Вчера";
        return at.ToString(at.Year == today.Year ? "d MMMM" : "d MMMM yyyy", System.Globalization.CultureInfo.GetCultureInfo("ru-RU"));
    }

    /// A project's own tabs as segments: `chosen` is called with the tab's tag; returns the strip, a way
    /// to mark a tab chosen and a way to rename one (a count on it).
    private (FrameworkElement Strip, Action<string> Mark, Action<string, string> Rename) Segments(IReadOnlyList<(string Tag, string Text)> tabs, Action<string> chosen)
    {
        StackPanel row = new() { Orientation = Orientation.Horizontal, Spacing = 2 };
        Dictionary<string, Button> buttons = [];
        foreach ((string tag, string text) in tabs)
        {
            Button button = new() { Content = text, Style = (Style)Resources["WorkspaceSegmentStyle"] };
            button.Click += (_, _) => chosen(tag);
            buttons[tag] = button;
            row.Children.Add(button);
        }
        Border strip = new() { Style = (Style)Resources["WorkspaceSegmentsStyle"], Child = row };
        void Mark(string tag)
        {
            foreach ((string key, Button button) in buttons)
                button.Style = (Style)Resources[key == tag ? "WorkspaceSegmentOnStyle" : "WorkspaceSegmentStyle"];
        }
        void Rename(string tag, string text)
        {
            if (buttons.TryGetValue(tag, out Button? button)) button.Content = text;
        }
        return (strip, Mark, Rename);
    }
}
