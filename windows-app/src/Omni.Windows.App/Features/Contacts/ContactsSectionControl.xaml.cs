using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Omni.Windows.App.Features.Contacts;

public sealed partial class ContactsSectionControl : UserControl
{
    private string currentLayoutState = string.Empty;

    public ContactsSectionControl()
    {
        InitializeComponent();
        Loaded += OnContactsSectionLoaded;
        SizeChanged += OnContactsSectionSizeChanged;
        ContactsSectionRoot.RegisterPropertyChangedCallback(VisibilityProperty, OnContactsSectionRootVisibilityChanged);
        // The filter pills get their full width from the horizontal row even when the column clips
        // them, so their sizes say how wide the column has to be (Shared.SectionRail).
        foreach (UIElement pill in ContactsFilterRow.Children)
        {
            ((FrameworkElement)pill).SizeChanged += (_, _) => FitRailToFilters();
        }

        Loaded += (_, _) =>
        {
            Shared.SectionRail.Changed -= ApplyRailWidth;
            Shared.SectionRail.Changed += ApplyRailWidth;
            ApplyRailWidth();
        };
        Unloaded += (_, _) => Shared.SectionRail.Changed -= ApplyRailWidth;
    }

    private void FitRailToFilters()
    {
        double row = ContactsFilterRow.Spacing * (ContactsFilterRow.Children.Count - 1);
        foreach (UIElement pill in ContactsFilterRow.Children)
        {
            row += ((FrameworkElement)pill).ActualWidth;
        }

        Shared.SectionRail.Fit(row + ContactsLeftPane.Padding.Left + ContactsLeftPane.Padding.Right +
            ContactsLeftPane.BorderThickness.Left + ContactsLeftPane.BorderThickness.Right);
    }

    // Only the two-column layout has a rail; the narrow one is the list alone, full width.
    private void ApplyRailWidth()
    {
        if (currentLayoutState == "ContactsWide")
        {
            ContactsPrimaryColumn.Width = new GridLength(Shared.SectionRail.Width);
        }
    }

    private void OnContactsSectionLoaded(object sender, RoutedEventArgs e)
        => ApplyResponsiveLayout(ActualWidth, force: true);

    private void OnContactsSectionSizeChanged(object sender, SizeChangedEventArgs e)
        => ApplyResponsiveLayout(e.NewSize.Width);

    private void OnContactsSectionRootVisibilityChanged(DependencyObject sender, DependencyProperty dp)
    {
        if (ContactsSectionRoot.Visibility != Visibility.Visible)
        {
            return;
        }

        DispatcherQueue.TryEnqueue(() => ApplyResponsiveLayout(ActualWidth, force: true));
    }

    // Drive list/detail layout off THIS control's width, not the window width: the
    // section is hosted inside NavigationView.Content (pane eats up to 228px), so an
    // AdaptiveTrigger on MinWindowWidth over-reports available space and keeps the
    // 392px primary column too wide at half-screen (см. docs/architecture/ui-audit.md §D).
    private void ApplyResponsiveLayout(double width, bool force = false)
    {
        // Two columns from the same width as the chats screen, and then the list is as wide as
        // everywhere (Shared.SectionRail) (owner, 2026-10-11: «всё равно разная ширина»): the 760-980 band used to
        // stack the list over the card.
        string target = width >= 720 ? "ContactsWide" : "ContactsNarrow";

        if (!force && target == currentLayoutState)
        {
            return;
        }

        currentLayoutState = target;
        VisualStateManager.GoToState(this, target, useTransitions: false);
        ApplyRailWidth();
    }
}
