namespace Omni.Windows.App.Features.Shared;

/// <summary>
/// The panel of details beside a conversation (a contact's or a group's page): one element, one width
/// (the person's, <see cref="ColumnWidth.Details"/>) and one rule. The person opens it from the header; below <see cref="SideBySideFrom"/> it covers the
/// conversation instead of squeezing it (docs/decisions/omni-app-ux.md R5).
/// </summary>
internal static class DetailsPanel
{
    // What the conversation keeps beside the panel, at the least.
    private const double ConversationAtLeast = 400;

    internal static double Width => ColumnWidth.Details.Width;

    internal static double SideBySideFrom => Width + ConversationAtLeast;

    /// <summary>The panel's left edge, to drag; the layout shows it only while the panel stands beside
    /// the conversation.</summary>
    internal static RailSplitter AddEdge(Microsoft.UI.Xaml.Controls.Grid panel)
    {
        RailSplitter edge = new() { Source = ColumnWidth.Details, OnLeft = true };
        Microsoft.UI.Xaml.Controls.Grid.SetRowSpan(edge, 10);
        panel.Children.Add(edge);
        return edge;
    }
}
