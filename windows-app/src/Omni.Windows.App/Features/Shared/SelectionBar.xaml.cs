using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Omni.Windows.App.Features.Shared;

/// See SelectionBar.xaml.
public sealed partial class SelectionBar : UserControl
{
    public SelectionBar() => InitializeComponent();

    public event EventHandler? CopyRequested;

    public event EventHandler? ForwardRequested;

    public event EventHandler? DeleteRequested;

    public event EventHandler? CancelRequested;

    /// None picked hides it.
    internal void ShowCount(int count, bool selecting)
    {
        Visibility = selecting ? Visibility.Visible : Visibility.Collapsed;
        CountPart.Text = $"Выбрано: {count}";
    }

    private void OnCopyClicked(object sender, RoutedEventArgs e) => CopyRequested?.Invoke(this, EventArgs.Empty);

    private void OnForwardClicked(object sender, RoutedEventArgs e) => ForwardRequested?.Invoke(this, EventArgs.Empty);

    private void OnDeleteClicked(object sender, RoutedEventArgs e) => DeleteRequested?.Invoke(this, EventArgs.Empty);

    private void OnCancelClicked(object sender, RoutedEventArgs e) => CancelRequested?.Invoke(this, EventArgs.Empty);
}

/// Which messages of a feed are picked. The feed's rows come and go on refresh; the ids stay.
internal sealed class MessageSelection(Func<IEnumerable<MessageRow>> rows, SelectionBar bar)
{
    private readonly HashSet<string> picked = new(StringComparer.Ordinal);
    private bool selecting;

    internal bool IsSelecting => selecting;

    /// The picked rows, in the feed's order.
    internal List<MessageRow> Picked => rows().Where(r => picked.Contains(r.MessageId)).ToList();

    internal void Start(MessageRow first)
    {
        selecting = true;
        picked.Clear();
        picked.Add(first.MessageId);
        Apply();
    }

    internal void Toggle(MessageRow row)
    {
        if (!picked.Remove(row.MessageId))
        {
            picked.Add(row.MessageId);
        }

        Apply();
    }

    internal void Stop()
    {
        selecting = false;
        picked.Clear();
        Apply();
    }

    /// Puts the state on the rows; also after a refresh made new ones.
    internal void Apply()
    {
        foreach (MessageRow row in rows())
        {
            row.SetSelection(selecting, picked.Contains(row.MessageId));
        }

        bar.ShowCount(picked.Count, selecting);
    }
}
