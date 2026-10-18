namespace Omni.Windows.App.Features.Shared;

/// <summary>
/// The width of the left column, one for every section (chats, contacts, settings, diagnostics), so
/// the title rows do not jump when switching tabs (owner, 2026-10-11: «разная ширина колонки»).
///
/// Exactly what the contact filters need, so the gap after the last pill's text equals the gap
/// before the first one's — the panel's padding plus the pill's (owner, 2026-10-12: «чтоб справа
/// было одинаковое расстояние как слева»). 275 with one-digit counts; it grows when the filters
/// need more — two- and three-digit counts (owner: «расширяющейся, если цифры будут становиться
/// двухзначными»; counts stop at «999+»). It never shrinks below 275.
/// </summary>
internal static class SectionRail
{
    internal const double MinWidth = 275;

    internal static double Width { get; private set; } = MinWidth;

    internal static event Action? Changed;

    /// <summary>The column has to hold a row this wide (the panel's padding and border included).</summary>
    internal static void Fit(double requiredWidth)
    {
        double width = Math.Max(MinWidth, Math.Ceiling(requiredWidth));
        if (Math.Abs(width - Width) < 0.5)
        {
            return;
        }

        Width = width;
        Changed?.Invoke();
    }
}
