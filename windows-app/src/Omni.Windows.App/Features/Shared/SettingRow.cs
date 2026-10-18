using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Markup;

namespace Omni.Windows.App.Features.Shared;

/// <summary>
/// One line of a settings page: what it is on the left (a title, a line under it), the control on the
/// right (what is written inside the row in XAML), and — when it needs room — more under both
/// (<see cref="Details"/>: a list, a small form). A hairline under each row, no box (owner, 2026-10-12:
/// «отказаться от таких пузыриков»; 2026-10-13: the right side of Settings and Diagnostics remade as rows
/// instead of walls of text and full-width fields).
/// Built in code, not templated: a ControlTemplate version crashed the page on measure (E_INVALIDARG
/// from the base MeasureOverride, live 2026-10-12 22:52).
/// </summary>
[ContentProperty(Name = nameof(Control))]
public sealed class SettingRow : UserControl
{
    private readonly Grid grid = new()
    {
        Padding = new Thickness(0, 11, 0, 11),
        ColumnSpacing = 24,
        BorderThickness = new Thickness(0, 0, 0, 1),
    };

    private readonly TextBlock header = new() { TextWrapping = TextWrapping.WrapWholeWords };

    private readonly TextBlock description = new()
    {
        FontSize = 12.5,
        Opacity = 0.66,
        TextWrapping = TextWrapping.WrapWholeWords,
        Visibility = Visibility.Collapsed,
    };

    private UIElement? control;
    private UIElement? details;

    public SettingRow()
    {
        grid.BorderBrush = (Microsoft.UI.Xaml.Media.Brush)Application.Current.Resources["OmniStrokeBrush"];
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        grid.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        grid.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        var text = new StackPanel { VerticalAlignment = VerticalAlignment.Center, Spacing = 2 };
        text.Children.Add(header);
        text.Children.Add(description);
        grid.Children.Add(text);
        Content = grid;
    }

    public string Header
    {
        get => header.Text;
        set => header.Text = value;
    }

    /// The line under the title: what the setting does. Empty — no line.
    public string Description
    {
        get => description.Text;
        set
        {
            description.Text = value;
            description.Visibility = string.IsNullOrEmpty(value) ? Visibility.Collapsed : Visibility.Visible;
        }
    }

    /// The control on the right (a button, a switch, a list box).
    public UIElement? Control
    {
        get => control;
        set
        {
            Replace(ref control, value);
            if (value is FrameworkElement element)
            {
                element.VerticalAlignment = VerticalAlignment.Center;
                element.HorizontalAlignment = HorizontalAlignment.Right;
            }
            if (value is not null)
            {
                Grid.SetColumn((FrameworkElement)value, 1);
            }
        }
    }

    /// Full-width content under the row (a list, a small form). Null — nothing.
    public UIElement? Details
    {
        get => details;
        set
        {
            Replace(ref details, value);
            if (value is FrameworkElement element)
            {
                Grid.SetRow(element, 1);
                Grid.SetColumnSpan(element, 2);
                // Under the row, close to its title.
                if (element.Margin == default)
                {
                    element.Margin = new Thickness(0, 6, 0, 0);
                }
                // A status line that says nothing yet takes no room (it left a gap under the row).
                if (element is TextBlock line)
                {
                    void Fit() => line.Visibility = string.IsNullOrEmpty(line.Text) ? Visibility.Collapsed : Visibility.Visible;
                    Fit();
                    line.RegisterPropertyChangedCallback(TextBlock.TextProperty, (_, _) => Fit());
                }
            }
        }
    }

    private void Replace(ref UIElement? slot, UIElement? value)
    {
        if (slot is not null)
        {
            grid.Children.Remove(slot);
        }
        slot = value;
        if (value is not null)
        {
            grid.Children.Add(value);
        }
    }
}
