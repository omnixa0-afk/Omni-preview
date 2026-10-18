using System;
using System.Collections.Generic;
using System.Collections.ObjectModel;
using System.ComponentModel;
using System.Linq;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Media.Imaging;

namespace Omni.Windows.App.Features.Groups;

/// Where a contact stands with the group being filled: free to invite, already in it, or invited by
/// us and not yet answered. The last two are shown but cannot be picked again.
public enum GroupPickState
{
    // Declaration order is the list order.
    Available,
    Invited,
    Member,
}

/// One contact row in the people picker (approved contacts only — an invitation travels over 1:1).
public sealed class GroupPickCandidate : INotifyPropertyChanged
{
    private bool isSelected;

    public string DialogKey { get; init; } = string.Empty;
    public string AccountId { get; init; } = string.Empty;
    public string Title { get; init; } = string.Empty;
    public string Initials { get; init; } = string.Empty;
    public GroupPickState State { get; init; }

    // Visuals copied from the contact's chat row (same avatar and presence dot as the chat list).
    public BitmapImage? AvatarImage { get; init; }
    public Visibility AvatarImageVisibility { get; init; } = Visibility.Collapsed;
    public Visibility PresenceDotVisibility { get; init; } = Visibility.Collapsed;
    public Brush? PresenceDotBrush { get; init; }

    public bool IsSelectable => State == GroupPickState.Available;

    public string StateText => State switch
    {
        GroupPickState.Member => "уже в группе",
        GroupPickState.Invited => "приглашение отправлено · ждём ответа",
        _ => string.Empty,
    };

    public Visibility StateTextVisibility => StateText.Length > 0 ? Visibility.Visible : Visibility.Collapsed;
    public double RowOpacity => IsSelectable ? 1.0 : 0.55;

    public bool IsSelected
    {
        get => isSelected;
        internal set
        {
            if (isSelected == value)
            {
                return;
            }
            isSelected = value;
            PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(nameof(IsSelected)));
            PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(nameof(CheckVisibility)));
        }
    }

    public Visibility CheckVisibility => isSelected ? Visibility.Visible : Visibility.Collapsed;

    public event PropertyChangedEventHandler? PropertyChanged;
}

/// The picker's state without any UI: search, selection (in pick order — the chips), and the texts
/// the dialog buttons show. Used for both "new group" and "add to group".
public sealed class GroupPeoplePickerModel
{
    private readonly List<GroupPickCandidate> all;

    public GroupPeoplePickerModel(IEnumerable<GroupPickCandidate> candidates)
    {
        // Who can be picked comes first (A→Z), then the ones already invited, then members.
        all = candidates
            .OrderBy(c => c.State)
            .ThenBy(c => c.Title, StringComparer.CurrentCultureIgnoreCase)
            .ToList();
        foreach (GroupPickCandidate c in all)
        {
            Visible.Add(c);
        }
    }

    /// Rows matching the search, in display order.
    public ObservableCollection<GroupPickCandidate> Visible { get; } = new();

    /// Picked contacts in the order they were picked.
    public ObservableCollection<GroupPickCandidate> Selected { get; } = new();

    public bool HasAnyCandidate => all.Count > 0;
    public bool HasAnySelectable => all.Any(c => c.IsSelectable);

    public IReadOnlyList<string> SelectedDialogKeys => Selected.Select(c => c.DialogKey).ToList();

    public event Action? SelectionChanged;

    public void SetQuery(string? query)
    {
        string q = (query ?? string.Empty).Trim();
        List<GroupPickCandidate> match = q.Length == 0
            ? all
            : all.Where(c => c.Title.Contains(q, StringComparison.CurrentCultureIgnoreCase)).ToList();
        if (match.SequenceEqual(Visible))
        {
            return;
        }
        Visible.Clear();
        foreach (GroupPickCandidate c in match)
        {
            Visible.Add(c);
        }
    }

    /// Pick or unpick. Members and already-invited contacts are not pickable: `false`.
    public bool Toggle(GroupPickCandidate candidate)
    {
        if (!candidate.IsSelectable)
        {
            return false;
        }
        candidate.IsSelected = !candidate.IsSelected;
        if (candidate.IsSelected)
        {
            Selected.Add(candidate);
        }
        else
        {
            Selected.Remove(candidate);
        }
        SelectionChanged?.Invoke();
        return true;
    }

    public static GroupPickState StateFor(
        string dialogKey,
        string accountId,
        IReadOnlySet<string> memberAccountIds,
        IReadOnlySet<string> invitedDialogKeys)
    {
        if (accountId.Length > 0 && memberAccountIds.Contains(accountId))
        {
            return GroupPickState.Member;
        }
        return invitedDialogKeys.Contains(dialogKey) ? GroupPickState.Invited : GroupPickState.Available;
    }

    public static string InviteButtonText(int count) => count == 0 ? "Пригласить" : $"Пригласить ({count})";

    // Step 1 of "new group": people are optional — a group can start with just you.
    public static string NextButtonText(int count) => count == 0 ? "Пропустить" : $"Далее ({count})";
}
