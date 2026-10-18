using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Omni.Windows.App.Features.Diagnostics;

public sealed partial class DiagnosticsSectionControl : UserControl
{
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

    /// Open a section by its tag (network, delivery, security, video, tools).
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
        string tag = (DiagnosticsNavList.SelectedItem as ListViewItem)?.Tag as string ?? "network";
        foreach (StackPanel section in new[]
                 {
                     DiagnosticsSection_network, DiagnosticsSection_delivery, DiagnosticsSection_security,
                     DiagnosticsSection_video, DiagnosticsSection_tools,
                 })
        {
            section.Visibility = section.Name == $"DiagnosticsSection_{tag}" ? Visibility.Visible : Visibility.Collapsed;
        }

        DiagnosticsContentScroll.ChangeView(null, 0, null, disableAnimation: true);
    }
}
