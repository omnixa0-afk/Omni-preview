using System.IO;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Omni.Windows.App.Features.Shared;

/// <summary>
/// The conversation header (see ConversationHeader.xaml). The host fills the parts and puts its
/// icon buttons in <see cref="Actions"/> (they keep their names in the host's XAML).
/// </summary>
public sealed partial class ConversationHeader : UserControl
{
    public static readonly DependencyProperty ActionsProperty = DependencyProperty.Register(
        nameof(Actions), typeof(object), typeof(ConversationHeader),
        new PropertyMetadata(null, (d, e) => ((ConversationHeader)d).ActionsPart.Content = e.NewValue));

    public ConversationHeader()
    {
        InitializeComponent();
        AvatarPart.TappedWithoutPicture += (_, _) => IdentityClicked?.Invoke(this, EventArgs.Empty);
    }

    /// The icon buttons at the right (call, «⋯»), the same 40 px ghost buttons everywhere.
    public object? Actions
    {
        get => GetValue(ActionsProperty);
        set => SetValue(ActionsProperty, value);
    }

    /// The narrow (phone-style) layout's way back to the list.
    public bool ShowBack
    {
        get => BackPart.Visibility == Visibility.Visible;
        set => BackPart.Visibility = value ? Visibility.Visible : Visibility.Collapsed;
    }

    public event EventHandler? BackRequested;

    /// A tap on the name (or on an avatar with no picture): the chat's page.
    public event EventHandler? IdentityClicked;


    public Button BackButton => BackPart;
    public Button IdentityButton => IdentityPart;
    public AvatarView Avatar => AvatarPart;
    public TextBlock Title => TitlePart;
    public TextBlock Subtitle => SubtitlePart;
    public TextBlock Detail => DetailPart;

    private void OnBackClicked(object sender, RoutedEventArgs e) => BackRequested?.Invoke(this, EventArgs.Empty);

    private void OnIdentityClicked(object sender, RoutedEventArgs e) => IdentityClicked?.Invoke(this, EventArgs.Empty);
}
