using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Windows.Foundation;

namespace Omni.Windows.App.Features.Shared;

/// <summary>
/// The body of a page: fills the window whatever its size. What comes before the first group is the
/// page's head; the groups (a group title and what follows it, or every child with
/// <see cref="EachChild"/>) stand in as many columns as fit, each column as wide as the room gives but
/// never wider than a row can be read across. One column on a small window, two or three on a large
/// one, and the same page beside a chat (docs/decisions/omni-app-ux.md, «Широкое окно»).
/// </summary>
internal sealed partial class PageColumns : Panel
{
    private readonly List<(UIElement Child, Rect At)> placed = [];

    public double Spacing { get; set; }

    public double ColumnGap { get; set; } = 40;

    public double MinColumn { get; set; } = 560;

    public double MaxColumn { get; set; } = 900;

    /// <summary>Every child is a group of its own: rows and cards with no titles over them.</summary>
    public bool EachChild { get; set; }

    protected override Size MeasureOverride(Size availableSize)
    {
        double room = double.IsInfinity(availableSize.Width) ? MaxColumn : availableSize.Width;
        List<UIElement> head = [];
        List<List<UIElement>> groups = [];
        foreach (UIElement child in Children)
        {
            if (EachChild || StartsGroup(child))
            {
                groups.Add([child]);
            }
            else if (groups.Count > 0)
            {
                groups[^1].Add(child);
            }
            else
            {
                head.Add(child);
            }
        }

        groups.RemoveAll(group => group.All(child => child.Visibility == Visibility.Collapsed));
        int columns = Math.Clamp((int)((room + ColumnGap) / (MinColumn + ColumnGap)), 1, Math.Max(1, groups.Count));
        double width = Math.Min(MaxColumn, (room - ColumnGap * (columns - 1)) / columns);
        double whole = width * columns + ColumnGap * (columns - 1);

        placed.Clear();
        double top = Stack(head, 0, 0, whole);
        double[] bottoms = new double[columns];
        Array.Fill(bottoms, top);
        foreach (List<UIElement> group in groups)
        {
            // The shortest column takes the next group, so the page reads left to right, top to bottom.
            int column = Array.IndexOf(bottoms, bottoms.Min());
            bottoms[column] = Stack(group, column * (width + ColumnGap), bottoms[column], width);
        }

        return new Size(whole, Math.Max(top, bottoms.Max()));
    }

    protected override Size ArrangeOverride(Size finalSize)
    {
        foreach ((UIElement child, Rect at) in placed)
        {
            child.Arrange(at);
        }

        return finalSize;
    }

    private double Stack(List<UIElement> children, double left, double top, double width)
    {
        foreach (UIElement child in children)
        {
            child.Measure(new Size(width, double.PositiveInfinity));
            double height = child.DesiredSize.Height;
            placed.Add((child, new Rect(left, top, width, height)));
            if (child.Visibility != Visibility.Collapsed)
            {
                top += height + Spacing;
            }
        }

        return top;
    }

    private static bool StartsGroup(UIElement child) =>
        child is FrameworkElement { Tag: "page-group" }
        || (child is TextBlock title && Application.Current.Resources.TryGetValue("SettingGroupTitleTextStyle", out object style) && ReferenceEquals(title.Style, style));
}
