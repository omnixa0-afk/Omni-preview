using Microsoft.UI.Xaml.Controls;

namespace Omni.Windows.App.Features.Onboarding;

public sealed partial class OnboardingShellControl : UserControl
{
    public OnboardingShellControl()
    {
        InitializeComponent();
        // The status box speaks only when there is something to say.
        OnboardingStatusTextBlock.RegisterPropertyChangedCallback(TextBlock.TextProperty, (_, _) =>
            OnboardingStatusBorder.Visibility = string.IsNullOrWhiteSpace(OnboardingStatusTextBlock.Text)
                ? Microsoft.UI.Xaml.Visibility.Collapsed
                : Microsoft.UI.Xaml.Visibility.Visible);
    }
}
