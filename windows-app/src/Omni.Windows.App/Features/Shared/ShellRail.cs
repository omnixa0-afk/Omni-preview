using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;

namespace Omni.Windows.App.Features.Shared;

/// <summary>
/// The sections' rail: an icon with its name under it, always (docs/decisions/omni-app-ux.md §2 — a
/// person new to the app read nothing from bare icons, and a `{}` said nothing at all). The items sit
/// anywhere inside it (the top group, the bottom one); choosing one is <see cref="SelectedItem"/>, as
/// the click and the code set it alike.
/// </summary>
internal sealed partial class ShellRail : Grid
{
    private ShellRailItem? selected;

    internal event EventHandler<ShellRailSelectionChangedEventArgs>? SelectionChanged;

    internal ShellRailItem? SelectedItem
    {
        get => selected;
        set
        {
            if (ReferenceEquals(selected, value))
            {
                return;
            }

            if (selected is not null)
            {
                selected.IsCurrent = false;
            }

            selected = value;
            if (selected is not null)
            {
                selected.IsCurrent = true;
            }

            SelectionChanged?.Invoke(this, new ShellRailSelectionChangedEventArgs(selected));
        }
    }
}

internal sealed class ShellRailSelectionChangedEventArgs(ShellRailItem? selectedItem) : EventArgs
{
    internal ShellRailItem? SelectedItem { get; } = selectedItem;
}

/// <summary>One section on the rail: its icon, its name under it, a count badge at the icon's corner.</summary>
internal sealed partial class ShellRailItem : Button
{
    private readonly Grid iconHost = new() { Width = 36, Height = 28, HorizontalAlignment = HorizontalAlignment.Center };
    private readonly TextBlock label = new()
    {
        FontSize = 11,
        HorizontalAlignment = HorizontalAlignment.Center,
        TextTrimming = TextTrimming.CharacterEllipsis,
        TextAlignment = TextAlignment.Center,
    };
    private IconElement? icon;
    private InfoBadge? infoBadge;
    private bool isCurrent;

    public ShellRailItem()
    {
        Width = 64;
        Height = 56;
        Padding = new Thickness(0);
        Margin = new Thickness(4, 2, 4, 2);
        CornerRadius = new CornerRadius(8);
        BorderThickness = new Thickness(0);
        Background = new SolidColorBrush(Microsoft.UI.Colors.Transparent);
        HorizontalContentAlignment = HorizontalAlignment.Stretch;
        VerticalContentAlignment = VerticalAlignment.Stretch;
        ActualThemeChanged += (_, _) => Paint();
        Click += (_, _) =>
        {
            DependencyObject? up = VisualTreeHelper.GetParent(this);
            while (up is not null and not ShellRail)
            {
                up = VisualTreeHelper.GetParent(up);
            }

            if (up is ShellRail rail)
            {
                rail.SelectedItem = this;
            }
        };
        StackPanel stack = new() { Spacing = 1, VerticalAlignment = VerticalAlignment.Center };
        stack.Children.Add(iconHost);
        stack.Children.Add(label);
        Content = stack;
        Paint();
    }

    public IconElement? Icon
    {
        get => icon;
        set
        {
            if (icon is not null)
            {
                iconHost.Children.Remove(icon);
            }

            icon = value;
            if (icon is not null)
            {
                icon.Width = 20;
                icon.Height = 20;
                icon.HorizontalAlignment = HorizontalAlignment.Center;
                icon.VerticalAlignment = VerticalAlignment.Center;
                iconHost.Children.Insert(0, icon);
            }

            Paint();
        }
    }

    public string Label
    {
        get => label.Text;
        set
        {
            label.Text = value;
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(this, value);
            ToolTipService.SetToolTip(this, value);
        }
    }

    /// <summary>A count on the icon's corner (unread chats, a new device of the account).</summary>
    public InfoBadge? InfoBadge
    {
        get => infoBadge;
        set
        {
            if (infoBadge is not null)
            {
                iconHost.Children.Remove(infoBadge);
            }

            infoBadge = value;
            if (infoBadge is not null)
            {
                infoBadge.HorizontalAlignment = HorizontalAlignment.Right;
                infoBadge.VerticalAlignment = VerticalAlignment.Top;
                infoBadge.Margin = new Thickness(0, -4, -6, 0);
                iconHost.Children.Add(infoBadge);
            }
        }
    }

    internal bool IsCurrent
    {
        get => isCurrent;
        set
        {
            isCurrent = value;
            Paint();
        }
    }

    // The current section: accent icon and name on the soft accent fill.
    private void Paint()
    {
        Brush transparent = new SolidColorBrush(Microsoft.UI.Colors.Transparent);
        Brush Themed(string key) => Presentation.MainWindowPresentation.ResolveThemeBrush(key, transparent);
        Brush text = Themed(isCurrent ? "OmniAccentBrush" : "OmniMutedTextBrush");
        label.Foreground = text;
        label.FontWeight = isCurrent ? Microsoft.UI.Text.FontWeights.SemiBold : Microsoft.UI.Text.FontWeights.Normal;
        if (icon is not null)
        {
            icon.Foreground = Themed(isCurrent ? "OmniAccentBrush" : "OmniTextBrush");
        }

        Background = isCurrent ? Themed("OmniAccentSoftBrush") : transparent;
    }
}
