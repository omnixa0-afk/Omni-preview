using Microsoft.UI.Input;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;

namespace Omni.Windows.App.Features.Shared;

/// <summary>
/// The list column's right edge, the same in every section: drag it to set <see cref="SectionRail"/>'s
/// width for all of them, double-click to go back to the default. Placed in the column's cell, aligned
/// right, straddling the column's border. Shown only while the column is a rail (an absolute width):
/// a narrow layout's single pane has no edge to drag. With <see cref="Source"/> it is the edge of a
/// column of that kind instead; with <see cref="OnLeft"/>, of a column on the right, its left edge.
/// </summary>
internal sealed partial class RailSplitter : Grid
{
    private const double Thickness = 8;
    private double startX;
    private double startWidth;
    private bool dragging;
    private bool onLeft;
    private ColumnWidth? source;

    /// <summary>The kind of column this edge belongs to; none: the sections' list.</summary>
    internal ColumnWidth? Source
    {
        get => source;
        set
        {
            source = value;
            ToolTipService.SetToolTip(this, ReferenceEquals(value, ColumnWidth.Details)
                ? "Ширина панели: потяните; двойной щелчок вернёт как было"
                : "Ширина списка: потяните; двойной щелчок вернёт как было");
        }
    }

    /// <summary>The column is to the right of the edge: dragging left widens it.</summary>
    internal bool OnLeft
    {
        get => onLeft;
        set
        {
            onLeft = value;
            Margin = value ? new Thickness(-Thickness / 2, 0, 0, 0) : new Thickness(0, 0, -Thickness / 2, 0);
            HorizontalAlignment = value ? HorizontalAlignment.Left : HorizontalAlignment.Right;
        }
    }

    private double Current => source?.Width ?? SectionRail.Width;

    private void Write(double width, bool done)
    {
        if (source is { } column)
        {
            column.Set(width, done);
        }
        else
        {
            SectionRail.Set(width, done);
        }
    }

    public RailSplitter()
    {
        Width = Thickness;
        Margin = new Thickness(0, 0, -Thickness / 2, 0);
        HorizontalAlignment = HorizontalAlignment.Right;
        VerticalAlignment = VerticalAlignment.Stretch;
        // Hit-testable, invisible.
        Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent);
        ProtectedCursor = InputSystemCursor.Create(InputSystemCursorShape.SizeWestEast);
        Canvas.SetZIndex(this, 10);
        ToolTipService.SetToolTip(this, "Ширина списка: потяните; двойной щелчок вернёт как было");
        PointerPressed += OnPressed;
        PointerMoved += OnMoved;
        PointerReleased += OnReleased;
        PointerCaptureLost += (_, _) => Finish();
        DoubleTapped += (_, e) =>
        {
            Write(source?.DefaultWidth ?? SectionRail.DefaultWidth, done: true);
            e.Handled = true;
        };
        LayoutUpdated += (_, _) => UpdateShown();
    }

    // The column this edge belongs to is a rail only with an absolute width.
    private void UpdateShown()
    {
        if (Parent is not Grid grid || grid.ColumnDefinitions.Count == 0)
        {
            return;
        }

        int column = Math.Min(GetColumn(this), grid.ColumnDefinitions.Count - 1);
        Visibility shown = grid.ColumnDefinitions[column].Width is { IsAbsolute: true, Value: > 0 } ? Visibility.Visible : Visibility.Collapsed;
        if (Visibility != shown)
        {
            Visibility = shown;
        }
    }

    private double X(PointerRoutedEventArgs e) => e.GetCurrentPoint(XamlRoot?.Content).Position.X;

    private void OnPressed(object sender, PointerRoutedEventArgs e)
    {
        if (!e.GetCurrentPoint(this).Properties.IsLeftButtonPressed)
        {
            return;
        }

        dragging = true;
        startX = X(e);
        startWidth = Current;
        CapturePointer(e.Pointer);
        e.Handled = true;
    }

    private void OnMoved(object sender, PointerRoutedEventArgs e)
    {
        if (!dragging)
        {
            return;
        }

        Write(startWidth + (onLeft ? startX - X(e) : X(e) - startX), done: false);
        e.Handled = true;
    }

    private void OnReleased(object sender, PointerRoutedEventArgs e)
    {
        ReleasePointerCapture(e.Pointer);
        Finish();
        e.Handled = true;
    }

    private void Finish()
    {
        if (!dragging)
        {
            return;
        }

        dragging = false;
        Write(Current, done: true);
    }
}
