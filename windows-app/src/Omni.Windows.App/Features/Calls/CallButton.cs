using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Omni.Windows.App.Features.Calls;

/// <summary>
/// The round button of a call's control bar — the same in the 1:1 call and the group call: a
/// 56 px circle with an icon and a short caption under it ("Микрофон", "Камера"). <see cref="IsOn"/>
/// fills it: the camera or screen is on — the state that differs from the default stands out.
/// <see cref="IsWarning"/> turns that fill red-tinted: the microphone is off, which must not read
/// as "camera on". The look is the implicit style in App.xaml.
/// </summary>
public sealed partial class CallButton : Button
{
    public static readonly DependencyProperty CaptionProperty = DependencyProperty.Register(
        nameof(Caption), typeof(string), typeof(CallButton), new PropertyMetadata(string.Empty));

    public static readonly DependencyProperty IsOnProperty = DependencyProperty.Register(
        nameof(IsOn), typeof(bool), typeof(CallButton), new PropertyMetadata(false, OnIsOnChanged));

    public static readonly DependencyProperty IsWarningProperty = DependencyProperty.Register(
        nameof(IsWarning), typeof(bool), typeof(CallButton), new PropertyMetadata(false, OnIsOnChanged));

    public string Caption
    {
        get => (string)GetValue(CaptionProperty);
        set => SetValue(CaptionProperty, value);
    }

    public bool IsOn
    {
        get => (bool)GetValue(IsOnProperty);
        set => SetValue(IsOnProperty, value);
    }

    /// <summary>The "on" state is a warning (the microphone is off), not an active feature.</summary>
    public bool IsWarning
    {
        get => (bool)GetValue(IsWarningProperty);
        set => SetValue(IsWarningProperty, value);
    }

    private string OnState => !IsOn ? "Off" : IsWarning ? "OnWarning" : "On";

    protected override void OnApplyTemplate()
    {
        base.OnApplyTemplate();
        VisualStateManager.GoToState(this, OnState, false);
    }

    private static void OnIsOnChanged(DependencyObject d, DependencyPropertyChangedEventArgs e)
    {
        var button = (CallButton)d;
        VisualStateManager.GoToState(button, button.OnState, true);
    }
}
