namespace Omni.Windows.App.Features.Shared;

/// <summary>
/// A column's width the person sets by dragging its edge (<see cref="RailSplitter"/>), one for all
/// columns of its kind, kept in the device preferences. The sections' own list has
/// <see cref="SectionRail"/>, which also has to fit the contact filters; these are the other two kinds.
/// </summary>
internal sealed class ColumnWidth(string preferenceKey, double min, double max, double defaultWidth)
{
    /// <summary>A list inside a page: discussions, a project's files, its versions.</summary>
    internal static readonly ColumnWidth PageList = new("pageListWidth", 260, 560, 340);

    /// <summary>The details beside a conversation (see <see cref="DetailsPanel"/>).</summary>
    internal static readonly ColumnWidth Details = new("detailsWidth", 300, 560, 360);

    internal static IReadOnlyList<ColumnWidth> All { get; } = [PageList, Details];

    private double chosen = defaultWidth;

    internal string PreferenceKey => preferenceKey;

    internal double DefaultWidth => defaultWidth;

    internal double Width => chosen;

    internal event Action? Changed;

    /// <summary>The person let go of the edge (or reset it): keep the width.</summary>
    internal event Action<double>? Chosen;

    /// <summary>The person's width: while dragging (<paramref name="done"/> false) and when let go.</summary>
    internal void Set(double width, bool done)
    {
        double next = Math.Clamp(Math.Round(width), min, max);
        if (Math.Abs(next - chosen) >= 0.5)
        {
            chosen = next;
            Changed?.Invoke();
        }

        if (done)
        {
            Chosen?.Invoke(chosen);
        }
    }

    /// <summary>The kept width, at start: nothing to keep again.</summary>
    internal void Restore(double width)
    {
        chosen = Math.Clamp(width, min, max);
        Changed?.Invoke();
    }
}
