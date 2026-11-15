using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;

namespace Omni.Windows.App.Features.Shared;

/// See PinnedBar.xaml. The host hands it the conversation's pins, the latest first.
public sealed partial class PinnedBar : UserControl
{
    private IReadOnlyList<MessageReplyPreview> pins = [];
    private int shown;

    public PinnedBar() => InitializeComponent();

    /// Go to this message.
    public event EventHandler<string>? Opened;

    /// Take this message's pin away.
    public event EventHandler<string>? UnpinRequested;

    internal void Show(IReadOnlyList<MessageReplyPreview> next)
    {
        string? current = shown < pins.Count ? pins[shown].MessageId : null;
        pins = next;
        int at = current is null ? -1 : pins.ToList().FindIndex(p => p.MessageId == current);
        shown = at >= 0 ? at : 0;
        Draw();
    }

    private void Draw()
    {
        Visibility = pins.Count > 0 ? Visibility.Visible : Visibility.Collapsed;
        if (pins.Count == 0)
        {
            return;
        }

        TitlePart.Text = pins.Count > 1 ? $"Закреплённое сообщение {shown + 1} из {pins.Count}" : "Закреплённое сообщение";
        TextPart.Text = pins[shown].Text.Length > 0 ? pins[shown].Text : "Сообщение";
    }

    private void OnOpenClicked(object sender, RoutedEventArgs e)
    {
        if (pins.Count == 0)
        {
            return;
        }

        Opened?.Invoke(this, pins[shown].MessageId);
        shown = (shown + 1) % pins.Count;
        Draw();
    }

    private void OnUnpinClicked(object sender, RoutedEventArgs e)
    {
        if (shown < pins.Count)
        {
            UnpinRequested?.Invoke(this, pins[shown].MessageId);
        }
    }
}
