using System.Linq;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Omni.Windows.App.Features.Groups;

/// The people picker body (hosted in a ContentDialog by the shell). State lives in
/// <see cref="GroupPeoplePickerModel"/>; this only renders it and forwards clicks.
public sealed partial class GroupPeoplePicker : UserControl
{
    // One size for every step of the people dialogs (see the XAML note).
    public const double BodyWidth = 400;
    public const double BodyHeight = 440;

    public GroupPeoplePicker(GroupPeoplePickerModel model)
    {
        Model = model;
        InitializeComponent();
        Root.Width = BodyWidth;
        Root.Height = BodyHeight;
        CandidatesList.ItemsSource = model.Visible;
        ChipsRepeater.ItemsSource = model.Selected;
        model.SelectionChanged += OnSelectionChanged;
        model.Visible.CollectionChanged += (_, _) => UpdateEmptyText();
        UpdateEmptyText();
        Loaded += (_, _) => SearchBox.Focus(FocusState.Programmatic);
    }

    public GroupPeoplePickerModel Model { get; }

    private void OnSearchChanged(object sender, TextChangedEventArgs e) => Model.SetQuery(SearchBox.Text);

    private void OnCandidateClicked(object sender, ItemClickEventArgs e)
    {
        if (e.ClickedItem is GroupPickCandidate candidate)
        {
            Model.Toggle(candidate);
        }
    }

    private void OnChipClicked(object sender, RoutedEventArgs e)
    {
        if (sender is FrameworkElement { Tag: string dialogKey }
            && Model.Selected.FirstOrDefault(c => c.DialogKey == dialogKey) is { } candidate)
        {
            Model.Toggle(candidate);
        }
    }

    private void OnSelectionChanged()
    {
        ChipsScroll.Visibility = Model.Selected.Count > 0 ? Visibility.Visible : Visibility.Collapsed;
        // Keep the newest chip in view.
        DispatcherQueue.TryEnqueue(() => ChipsScroll.ChangeView(ChipsScroll.ScrollableWidth, null, null, true));
    }

    private void UpdateEmptyText()
    {
        string? text = !Model.HasAnyCandidate
            ? "Контактов пока нет. Добавьте человека в «Контактах» — после этого его можно будет пригласить."
            : Model.Visible.Count == 0
                ? "Никого не нашлось."
                : null;
        EmptyText.Text = text ?? string.Empty;
        EmptyText.Visibility = text is null ? Visibility.Collapsed : Visibility.Visible;
    }
}
