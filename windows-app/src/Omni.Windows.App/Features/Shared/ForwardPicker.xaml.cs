using System.ComponentModel;
using Microsoft.UI.Input;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Media.Imaging;
using Omni.Windows.App.Presentation;
using Windows.System;
using Windows.UI.Core;

namespace Omni.Windows.App.Features.Shared;

/// One conversation messages can be forwarded to. Plain data until drawn: it is listed off the UI thread.
public sealed class ForwardChoice : INotifyPropertyChanged
{
    private bool picked;

    public string Title { get; init; } = string.Empty;

    public string Subtitle { get; init; } = string.Empty;

    public string DialogKey { get; init; } = string.Empty;

    public string? GroupId { get; init; }

    public string? AvatarPath { get; init; }

    public string? AvatarVersion { get; init; }

    public string AvatarSeed { get; init; } = string.Empty;

    public long SortUnixMs { get; init; }

    public event PropertyChangedEventHandler? PropertyChanged;

    public string Initials => MainWindowPresentation.BuildAvatarText(Title);

    public Brush AvatarBrush => AvatarPalette.BrushFor(AvatarSeed);

    public BitmapImage? AvatarImage => MainWindowPresentation.BuildAvatarImage(AvatarPath, 96, AvatarVersion);

    public Visibility PickedVisibility => picked ? Visibility.Visible : Visibility.Collapsed;

    internal bool Picked
    {
        get => picked;
        set
        {
            picked = value;
            PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(nameof(PickedVisibility)));
        }
    }

    public override string ToString() => Title;
}

/// See ForwardPicker.xaml.
public sealed partial class ForwardPicker : UserControl
{
    private const int TabPeople = 1;
    private const int TabGroups = 2;

    private readonly List<ForwardChoice> choices;

    internal ForwardPicker(List<ForwardChoice> choices)
    {
        this.choices = choices;
        InitializeComponent();
        ShowMatching();
    }

    /// A press with nothing marked: send there.
    internal event Action<ForwardChoice>? SendTo;

    internal event Action? PickedChanged;

    internal List<ForwardChoice> Picked => choices.Where(c => c.Picked).ToList();

    internal void FocusSearch() => SearchPart.Focus(FocusState.Programmatic);

    private void OnSearchChanged(object sender, TextChangedEventArgs e) => ShowMatching();

    private void OnTabChanged(SelectorBar sender, SelectorBarSelectionChangedEventArgs args) => ShowMatching();

    private void ShowMatching()
    {
        string wanted = SearchPart.Text.Trim();
        int tab = TabsPart.SelectedItem is { } item ? TabsPart.Items.IndexOf(item) : 0;
        List<ForwardChoice> shown = choices
            .Where(c => tab != TabPeople || c.GroupId is null)
            .Where(c => tab != TabGroups || c.GroupId is not null)
            .Where(c => c.Title.Contains(wanted, StringComparison.CurrentCultureIgnoreCase))
            .ToList();
        ListPart.ItemsSource = shown;
        EmptyPart.Visibility = shown.Count == 0 ? Visibility.Visible : Visibility.Collapsed;
    }

    private void OnRowClicked(object sender, ItemClickEventArgs e)
    {
        ForwardChoice row = (ForwardChoice)e.ClickedItem;
        bool ctrl = InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Control).HasFlag(CoreVirtualKeyStates.Down);
        if (!ctrl && !choices.Any(c => c.Picked))
        {
            SendTo?.Invoke(row);
            return;
        }

        row.Picked = !row.Picked;
        PickedChanged?.Invoke();
    }
}
