namespace Omni.Windows.App.Features.Shared;

/// <summary>
/// The width of the list column, one for every section (chats, contacts, workspaces, settings,
/// diagnostics): it is one element with different content, so the title rows do not jump when
/// switching sections (owner, 2026-10-11: «разная ширина колонки»), and the person sets it once by
/// dragging its edge (<see cref="RailSplitter"/>; owner, 2026-11-03: «ширина колонки должна
/// сохраняться везде»). The choice is kept in the device preferences.
///
/// It never goes below what the contact filters need, so the gap after the last pill's text equals
/// the gap before the first one's (owner, 2026-10-12); that floor grows with two- and three-digit
/// counts (counts stop at «999+»).
/// </summary>
internal static class SectionRail
{
    internal const double MinWidth = 275;
    internal const double MaxWidth = 480;
    internal const double DefaultWidth = 300;

    private static double chosen = DefaultWidth;
    private static double floor = MinWidth;

    internal static double Width => Math.Clamp(chosen, floor, MaxWidth);

    internal static event Action? Changed;

    /// <summary>The person let go of the edge (or reset it): keep the width.</summary>
    internal static event Action<double>? Chosen;

    /// <summary>The column has to hold a row this wide (the panel's padding and border included).</summary>
    internal static void Fit(double requiredWidth)
    {
        double before = Width;
        floor = Math.Max(MinWidth, Math.Ceiling(requiredWidth));
        if (Math.Abs(Width - before) >= 0.5)
        {
            Changed?.Invoke();
        }
    }

    /// <summary>The person's width: while dragging (<paramref name="done"/> false) and when let go.</summary>
    internal static void Set(double width, bool done)
    {
        double before = Width;
        chosen = Math.Clamp(Math.Round(width), MinWidth, MaxWidth);
        if (Math.Abs(Width - before) >= 0.5)
        {
            Changed?.Invoke();
        }

        if (done)
        {
            Chosen?.Invoke(chosen);
        }
    }

    /// <summary>The kept width, at start: nothing to keep again.</summary>
    internal static void Restore(double width)
    {
        chosen = Math.Clamp(width, MinWidth, MaxWidth);
        Changed?.Invoke();
    }
}
