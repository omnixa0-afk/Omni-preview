using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Windows.Foundation;

namespace Omni.Windows.App.Features.Shared;

/// <summary>
/// A row of buttons and filters that goes on to the next line when it does not fit, so nothing leaves
/// the window at a narrow width or a large display scale. Children of one line are centred on it.
/// </summary>
internal sealed partial class WrapRow : Panel
{
    public double Spacing { get; set; } = 8;

    protected override Size MeasureOverride(Size availableSize)
    {
        double width = 0, height = 0, lineWidth = 0, lineHeight = 0;
        foreach (UIElement child in Children)
        {
            child.Measure(availableSize);
            Size size = child.DesiredSize;
            if (size.Width <= 0)
            {
                continue;
            }

            if (lineWidth > 0 && lineWidth + Spacing + size.Width > availableSize.Width)
            {
                height += lineHeight + Spacing;
                width = Math.Max(width, lineWidth);
                lineWidth = 0;
                lineHeight = 0;
            }

            lineWidth += (lineWidth > 0 ? Spacing : 0) + size.Width;
            lineHeight = Math.Max(lineHeight, size.Height);
        }

        return new Size(Math.Max(width, lineWidth), height + lineHeight);
    }

    protected override Size ArrangeOverride(Size finalSize)
    {
        double top = 0;
        List<UIElement> line = [];
        double lineWidth = 0;

        void Place()
        {
            double lineHeight = line.Count == 0 ? 0 : line.Max(child => child.DesiredSize.Height);
            double left = 0;
            foreach (UIElement child in line)
            {
                Size size = child.DesiredSize;
                child.Arrange(new Rect(left, top + (lineHeight - size.Height) / 2, size.Width, size.Height));
                left += size.Width + Spacing;
            }

            top += lineHeight + Spacing;
            line.Clear();
            lineWidth = 0;
        }

        foreach (UIElement child in Children)
        {
            double childWidth = child.DesiredSize.Width;
            if (childWidth <= 0)
            {
                child.Arrange(new Rect(0, 0, 0, 0));
                continue;
            }

            if (line.Count > 0 && lineWidth + Spacing + childWidth > finalSize.Width)
            {
                Place();
            }

            lineWidth += (line.Count > 0 ? Spacing : 0) + childWidth;
            line.Add(child);
        }

        Place();
        return finalSize;
    }
}
