using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Media;
using Omni.Windows.App.Presentation;

namespace Omni.Windows.App.Features.Shared;

/// One the composer's list offers after `@`: a member, by account, since two may share a name.
public sealed class MentionCandidate(string account, string name, string note)
{
    public string Account => account;

    public string Name => name;

    /// Who this is when the name alone does not say: whose assistant it is.
    public string Note { get; internal set; } = note;

    public string Initial => name.Length > 0 ? name[..1].ToUpperInvariant() : "?";

    // UI thread only: the brush is shared per tone and follows the theme.
    public Brush AvatarBrush => AvatarPalette.BrushFor(account);

    public Visibility NoteVisibility => Note.Length > 0 ? Visibility.Visible : Visibility.Collapsed;

    /// The rows the list shows must tell namesakes apart even when nothing else differs: the start of
    /// the account then stands in the note, as it does in a contact's details.
    internal static IReadOnlyList<MentionCandidate> Distinct(IReadOnlyList<MentionCandidate> candidates)
    {
        foreach (IGrouping<string, MentionCandidate> same in candidates.GroupBy(c => $"{c.Name}\n{c.Note}".ToLowerInvariant()).Where(g => g.Count() > 1))
        {
            foreach (MentionCandidate candidate in same)
            {
                string start = candidate.Account.Length > 8 ? candidate.Account[..8] : candidate.Account;
                candidate.Note = candidate.Note.Length > 0 ? $"{candidate.Note} · {start}" : start;
            }
        }

        return candidates;
    }
}
