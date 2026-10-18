using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Omni.Windows.App.Features.Shared;

/// <summary>
/// Shows a diagnostics text («Ключ: значение» lines, several facts on a line joined by « • ») as rows —
/// the name on the left, the value on the right, a hairline under each — instead of a block of small
/// text (owner, 2026-10-13: the right side of Diagnostics remade). The builders keep writing the
/// source TextBlock (the test channel and the call service read it); it stays hidden and this follows
/// its text and its visibility.
/// </summary>
internal static class FactRows
{
    /// `source` sits in a collapsed holder in the page; its own Visibility is the code's say (the call
    /// service hides the media line when there is no call video) and the rows follow it.
    internal static void Mirror(TextBlock source, Panel target)
    {
        Render(source.Text, target);
        target.Visibility = source.Visibility;
        source.RegisterPropertyChangedCallback(TextBlock.TextProperty, (_, _) => Render(source.Text, target));
        source.RegisterPropertyChangedCallback(UIElement.VisibilityProperty, (_, _) => target.Visibility = source.Visibility);
    }

    private static void Render(string? text, Panel target)
    {
        target.Children.Clear();
        foreach (string line in (text ?? string.Empty).Split('\n', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries))
        {
            foreach (string fact in line.Split(" • ", StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries))
            {
                target.Children.Add(Row(fact));
            }
        }
    }

    private static Border Row(string fact)
    {
        int colon = fact.IndexOf(": ", StringComparison.Ordinal);
        var grid = new Grid { ColumnSpacing = 24 };
        if (colon <= 0 || colon > 48)
        {
            // Not a «name: value» fact — the whole line, as said.
            grid.Children.Add(new TextBlock { Text = fact, TextWrapping = TextWrapping.WrapWholeWords, IsTextSelectionEnabled = true });
        }
        else
        {
            grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(240) });
            grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            grid.Children.Add(new TextBlock
            {
                Text = fact[..colon],
                Opacity = 0.66,
                TextWrapping = TextWrapping.WrapWholeWords,
            });
            var value = new TextBlock
            {
                Text = fact[(colon + 2)..],
                TextWrapping = TextWrapping.Wrap,
                IsTextSelectionEnabled = true,
            };
            Grid.SetColumn(value, 1);
            grid.Children.Add(value);
        }

        return new Border
        {
            Style = (Style)Application.Current.Resources["SettingListRowStyle"],
            Padding = new Thickness(0, 8, 0, 8),
            Child = grid,
        };
    }
}
