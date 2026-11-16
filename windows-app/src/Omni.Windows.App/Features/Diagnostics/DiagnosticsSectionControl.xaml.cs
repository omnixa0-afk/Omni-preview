using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Omni.Windows.App.Features.Diagnostics;

public sealed partial class DiagnosticsSectionControl : UserControl
{
    /// A row of the help page asked to go somewhere: the tag of its button.
    internal event Action<string>? HelpActionRequested;

    public DiagnosticsSectionControl()
    {
        InitializeComponent();
        DiagnosticsNavList.SelectionChanged += (_, _) => ShowSelectedSection();
        DiagnosticsNavList.SelectedIndex = 0;
        Shared.FactRows.Mirror(TransportDiagnosticsTextBlock, TransportFactsPanel);
        Shared.FactRows.Mirror(QueueDiagnosticsTextBlock, QueueFactsPanel);
        Shared.FactRows.Mirror(SecurityDiagnosticsTextBlock, SecurityFactsPanel);
        Shared.FactRows.Mirror(CallVideoDiagnosticsTextBlock, CallVideoFactsPanel);
        Shared.FactRows.Mirror(ChatSessionStateTextBlock, ChatSessionStateFactsPanel);
        Shared.FactRows.Mirror(ChatSessionLatencyTextBlock, ChatSessionLatencyFactsPanel);
        Shared.FactRows.Mirror(ChatSessionNetworkTextBlock, ChatSessionNetworkFactsPanel);
        // One left-column width for every section (Shared.SectionRail).
        Loaded += (_, _) =>
        {
            Shared.SectionRail.Changed -= ApplyRailWidth;
            Shared.SectionRail.Changed += ApplyRailWidth;
            ApplyRailWidth();
        };
        Unloaded += (_, _) => Shared.SectionRail.Changed -= ApplyRailWidth;
    }

    private void ApplyRailWidth() => DiagnosticsNavColumn.Width = new GridLength(Shared.SectionRail.Width);

    internal void ShowHelpStatus(string title, string detail, string people, bool problem)
    {
        HelpStatusTitleTextBlock.Text = title;
        HelpStatusDetailTextBlock.Text = detail;
        HelpStatusPeopleTextBlock.Text = people;
        if (problem)
        {
            HelpStatusTitleTextBlock.Foreground = Presentation.MainWindowPresentation.ResolveThemeBrush(
                "OmniCallControlWarnIconBrush", HelpStatusDetailTextBlock.Foreground);
        }
        else
        {
            HelpStatusTitleTextBlock.ClearValue(TextBlock.ForegroundProperty);
        }
    }

    private string shownWaiting = string.Empty;

    /// The conversations with messages not gone yet, each with the way to it. Rows stay as they are
    /// until the list changes: a refresh must not replace a button under the mouse.
    internal void ShowHelpWaiting(IReadOnlyList<(string DialogKey, string Title, string Text)> waiting)
    {
        string now = string.Join('\n', waiting.Select(row => $"{row.DialogKey}\t{row.Title}\t{row.Text}"));
        if (now == shownWaiting)
        {
            return;
        }

        shownWaiting = now;
        HelpWaitingRows.Children.Clear();
        foreach ((string dialogKey, string title, string text) in waiting)
        {
            Button open = new() { Content = "Открыть", Tag = "chat:" + dialogKey };
            open.Click += OnHelpActionClicked;
            HelpWaitingRows.Children.Add(new Shared.SettingRow { Header = title, Description = text, Control = open });
        }

        HelpWaitingPanel.Visibility = waiting.Count > 0 ? Visibility.Visible : Visibility.Collapsed;
    }

    private void OnHelpActionClicked(object sender, RoutedEventArgs e)
    {
        if (sender is Button { Tag: string action })
        {
            HelpActionRequested?.Invoke(action);
        }
    }

    /// Open a section by its tag (help, network, delivery, security, video, tools).
    internal void ShowSection(string tag)
    {
        foreach (object item in DiagnosticsNavList.Items)
        {
            if (item is ListViewItem listed && Equals(listed.Tag, tag))
            {
                DiagnosticsNavList.SelectedItem = listed;
                return;
            }
        }
    }

    private void ShowSelectedSection()
    {
        string tag = (DiagnosticsNavList.SelectedItem as ListViewItem)?.Tag as string ?? "help";
        foreach (FrameworkElement section in new FrameworkElement[]
                 {
                     DiagnosticsSection_help, DiagnosticsSection_network, DiagnosticsSection_delivery, DiagnosticsSection_security,
                     DiagnosticsSection_video, DiagnosticsSection_tools,
                 })
        {
            section.Visibility = section.Name == $"DiagnosticsSection_{tag}" ? Visibility.Visible : Visibility.Collapsed;
        }

        DiagnosticsContentScroll.ChangeView(null, 0, null, disableAnimation: true);
    }
}
