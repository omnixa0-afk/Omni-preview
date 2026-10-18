using Microsoft.UI.Input;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Windows.System;
using Windows.UI.Core;

namespace Omni.Windows.App.Features.Shared;

/// <summary>
/// The composer (see MessageComposer.xaml). <see cref="SendRequested"/> comes from the button and
/// from Enter alike, and from Enter only while the send button is enabled; <see cref="Above"/>
/// takes the host's lines over the field.
/// </summary>
public sealed partial class MessageComposer : UserControl
{
    public static readonly DependencyProperty AboveProperty = DependencyProperty.Register(
        nameof(Above), typeof(object), typeof(MessageComposer),
        new PropertyMetadata(null, (d, e) => ((MessageComposer)d).AbovePart.Content = e.NewValue));

    public MessageComposer()
    {
        InitializeComponent();
    }

    public object? Above
    {
        get => GetValue(AboveProperty);
        set => SetValue(AboveProperty, value);
    }

    /// The paperclip; off where the conversation cannot take files (a group we are no longer in).
    public bool ShowAttach
    {
        get => AttachPart.Visibility == Visibility.Visible;
        set => AttachPart.Visibility = value ? Visibility.Visible : Visibility.Collapsed;
    }

    public event EventHandler? SendRequested;

    /// The cross on the file about to go.
    public event EventHandler? DraftRemoveRequested;

    /// «Сжать как фото» ticked (true) or unticked.
    public event EventHandler<bool>? DraftAsPhotoChanged;

    private bool showingDraft;

    /// The file about to go; a picture that can go as a photo shows itself and the tick.
    public void ShowDraft(string path, string name, string sizeText, bool photoCandidate, bool asPhoto)
    {
        showingDraft = true;
        DraftNamePart.Text = name;
        DraftSizePart.Text = sizeText;
        DraftAsPhotoPart.Visibility = photoCandidate ? Visibility.Visible : Visibility.Collapsed;
        DraftAsPhotoPart.IsChecked = asPhoto;
        DraftThumbnailPart.LocalPath = photoCandidate ? path : null;
        DraftThumbnailPart.Visibility = photoCandidate ? Visibility.Visible : Visibility.Collapsed;
        DraftFileIconPart.Visibility = photoCandidate ? Visibility.Collapsed : Visibility.Visible;
        DraftPart.Visibility = Visibility.Visible;
        showingDraft = false;
    }

    public void ClearDraft()
    {
        DraftPart.Visibility = Visibility.Collapsed;
        DraftThumbnailPart.LocalPath = null;
    }

    private void OnDraftRemoveClicked(object sender, RoutedEventArgs e) => DraftRemoveRequested?.Invoke(this, EventArgs.Empty);

    private void OnDraftAsPhotoToggled(object sender, RoutedEventArgs e)
    {
        if (!showingDraft)
        {
            DraftAsPhotoChanged?.Invoke(this, DraftAsPhotoPart.IsChecked == true);
        }
    }

    public event EventHandler? AttachRequested;

    public TextBox Input => InputPart;
    public Button SendButton => SendPart;
    public Button AttachButton => AttachPart;

    private void OnSendClicked(object sender, RoutedEventArgs e) => SendRequested?.Invoke(this, EventArgs.Empty);

    private void OnAttachClicked(object sender, RoutedEventArgs e) => AttachRequested?.Invoke(this, EventArgs.Empty);

    private void OnInputPreviewKeyDown(object sender, KeyRoutedEventArgs e)
    {
        if (e.Key != VirtualKey.Enter)
        {
            return;
        }

        bool shift = InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Shift).HasFlag(CoreVirtualKeyStates.Down);
        if (shift)
        {
            return; // a new line
        }

        e.Handled = true;
        if (SendPart.IsEnabled)
        {
            SendRequested?.Invoke(this, EventArgs.Empty);
        }
    }
}
