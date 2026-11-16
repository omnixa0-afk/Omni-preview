using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Shapes;

namespace Omni.Windows.App.Features.Shared;

/// A voice message's outline: bars, those already played in full colour, the rest faint. A tap says where.
public sealed partial class VoiceWaveform : Grid
{
    public static readonly DependencyProperty BarsProperty = DependencyProperty.Register(
        nameof(Bars), typeof(byte[]), typeof(VoiceWaveform), new PropertyMetadata(null, (d, _) => ((VoiceWaveform)d).Rebuild()));

    public static readonly DependencyProperty ProgressProperty = DependencyProperty.Register(
        nameof(Progress), typeof(double), typeof(VoiceWaveform), new PropertyMetadata(0d, (d, _) => ((VoiceWaveform)d).Paint()));

    public static readonly DependencyProperty BarBrushProperty = DependencyProperty.Register(
        nameof(BarBrush), typeof(Brush), typeof(VoiceWaveform), new PropertyMetadata(null, (d, _) => ((VoiceWaveform)d).Recolor()));

    private const int BarCount = 64;
    private const double BarWidth = 2;
    private const double BarGap = 1;
    private const double ShownHeight = 26;
    private const double LowestBar = 3;
    private const double FaintOpacity = 0.35;

    private readonly StackPanel row = new() { Orientation = Orientation.Horizontal, Spacing = BarGap, VerticalAlignment = VerticalAlignment.Center };
    private readonly List<Rectangle> shown = [];

    public VoiceWaveform()
    {
        Height = ShownHeight;
        HorizontalAlignment = HorizontalAlignment.Left;
        // The whole strip takes the tap, the gaps between the bars too.
        Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent);
        Children.Add(row);
        Tapped += OnTapped;
        Rebuild();
    }

    public byte[]? Bars
    {
        get => (byte[]?)GetValue(BarsProperty);
        set => SetValue(BarsProperty, value);
    }

    public double Progress
    {
        get => (double)GetValue(ProgressProperty);
        set => SetValue(ProgressProperty, value);
    }

    public Brush? BarBrush
    {
        get => (Brush?)GetValue(BarBrushProperty);
        set => SetValue(BarBrushProperty, value);
    }

    /// A tap: how far along, 0..1.
    public event EventHandler<double>? Sought;

    private void OnTapped(object sender, TappedRoutedEventArgs e)
    {
        double width = shown.Count * (BarWidth + BarGap);
        if (width > 0)
        {
            Sought?.Invoke(this, Math.Clamp(e.GetPosition(row).X / width, 0, 1));
            e.Handled = true;
        }
    }

    private void Rebuild()
    {
        byte[] bars = Bars ?? [];
        // No outline came with it: even bars, so the strip still reads as a voice message.
        int count = bars.Length > 0 ? bars.Length : BarCount;
        while (shown.Count < count)
        {
            Rectangle bar = new() { Width = BarWidth, RadiusX = 1, RadiusY = 1, VerticalAlignment = VerticalAlignment.Center, Fill = BarBrush };
            shown.Add(bar);
            row.Children.Add(bar);
        }

        while (shown.Count > count)
        {
            row.Children.RemoveAt(shown.Count - 1);
            shown.RemoveAt(shown.Count - 1);
        }

        for (int i = 0; i < count; i++)
        {
            double level = bars.Length > 0 ? bars[i] / 255d : 0.3;
            shown[i].Height = LowestBar + level * (ShownHeight - LowestBar);
        }

        Paint();
    }

    private void Paint()
    {
        double upTo = Progress * shown.Count;
        for (int i = 0; i < shown.Count; i++)
        {
            // The bar being played fills by degrees: the line flows instead of stepping bar by bar.
            shown[i].Opacity = FaintOpacity + (1 - FaintOpacity) * Math.Clamp(upTo - i, 0, 1);
        }
    }

    private void Recolor()
    {
        foreach (Rectangle bar in shown)
        {
            bar.Fill = BarBrush;
        }
    }
}
