using System.ComponentModel;

namespace Omni.Windows.App.Features.Shared;

/// How large a video message may be in this window: a low one must still show the feed around the circle.
public sealed class VideoNoteSizing : INotifyPropertyChanged
{
    // What the header, the composer and the title bar take from the window's height.
    private const double AroundTheFeed = 170;
    private const double ShareOfTheFeed = 0.6;
    private const double SmallestSide = 160;

    private double limit = double.MaxValue;

    public static VideoNoteSizing Instance { get; } = new();

    public event PropertyChangedEventHandler? PropertyChanged;

    public double Limit => limit;

    internal void WindowHeightChanged(double height)
    {
        double next = Math.Max(SmallestSide, (height - AroundTheFeed) * ShareOfTheFeed);
        if (next != limit)
        {
            limit = next;
            PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(nameof(Limit)));
        }
    }
}
