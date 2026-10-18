using Microsoft.UI.Xaml.Media.Imaging;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Groups;
using Omni.Windows.App.Presentation;

namespace Omni.Windows.App;

// How group members look: a member who is also our contact has their profile photo and the name we
// gave them in our contacts (the same person was «Laptop» in the chat list and «LaptopStand1» in the
// group — owner, 2026-10-13); we have our own photo. A member we have no contact with shows the name
// and photo from its own card in the group (docs/decisions/group-member-profile.md), a letter until
// one arrives. One image per account, built again only when its photo changes, so the group's rows
// and tiles are not redrawn on every refresh.
public sealed partial class MainWindow
{
    private readonly Dictionary<string, (string Key, BitmapImage Image)> memberPhotos = new(StringComparer.Ordinal);
    private readonly Dictionary<string, string> memberNames = new(StringComparer.Ordinal);
    // Accounts we have as contacts: their look comes from the contact, never from a group card.
    private readonly HashSet<string> contactAccounts = new(StringComparer.Ordinal);
    // A non-contact member's name from its group card.
    private readonly Dictionary<string, string> memberCardNames = new(StringComparer.Ordinal);

    /// Take the contacts' photos and names from a chat-list refresh; tell the open group when one changed.
    private void NoteContactPhotos(IEnumerable<ContactSnapshot> contacts)
    {
        bool changed = false;
        foreach (ContactSnapshot contact in contacts)
        {
            if (contact.RemovedAtUnixMs.HasValue || string.IsNullOrWhiteSpace(contact.RemoteAccountId))
            {
                continue;
            }

            string accountId = contact.RemoteAccountId!;
            contactAccounts.Add(accountId);
            changed |= NoteMemberPhoto(accountId, contact.PublicAvatarImagePath, BuildContactAvatarVersionToken(contact));
            string name = MainWindowPresentation.ContactTitle(contact);
            if (!memberNames.TryGetValue(accountId, out string? known) || known != name)
            {
                memberNames[accountId] = name;
                changed = true;
            }
        }

        if (changed)
        {
            GroupConversation.MemberPhotosChanged();
        }
    }

    /// The open group's members as the core sees them: for the ones who are not our contacts, the name
    /// and photo from their own card. Tells the group when one changed.
    private void NoteGroupMemberCards(IReadOnlyList<GroupMemberView> members)
    {
        bool changed = false;
        foreach (GroupMemberView member in members)
        {
            if (member.IsMe || string.IsNullOrEmpty(member.AccountId) || contactAccounts.Contains(member.AccountId))
            {
                continue;
            }

            // The card's photo file is named by its sha256: the path is its version.
            changed |= NoteMemberPhoto(member.AccountId, member.AvatarPath, member.AvatarPath);
            if (!memberCardNames.TryGetValue(member.AccountId, out string? known) || known != member.DisplayName)
            {
                memberCardNames[member.AccountId] = member.DisplayName;
                changed = true;
            }
        }

        if (changed)
        {
            GroupConversation.MemberPhotosChanged();
        }
    }

    // A photo for an account: built only when its path or version changed. True when it changed.
    private bool NoteMemberPhoto(string accountId, string? path, string? versionToken)
    {
        string key = $"{path}|{versionToken}";
        if (memberPhotos.TryGetValue(accountId, out (string Key, BitmapImage Image) known) && known.Key == key)
        {
            return false;
        }

        BitmapImage? image = MainWindowPresentation.BuildAvatarImage(path, 96, versionToken);
        if (image is null)
        {
            return memberPhotos.Remove(accountId);
        }

        memberPhotos[accountId] = (key, image);
        return true;
    }

    /// The member's photo, our name for them, and what identifies both (rows redraw when it changes).
    private MemberLook ResolveMember(string accountId)
    {
        LocalAccountSnapshot? me = windowState.Onboarding.LocalAccount;
        if (me is not null && string.Equals(me.Descriptor.AccountId, accountId, StringComparison.Ordinal))
        {
            NoteMemberPhoto(accountId, me.PublicAvatarImagePath, BuildLocalAvatarVersionToken(me));
        }

        if (!memberNames.TryGetValue(accountId, out string? name))
        {
            memberCardNames.TryGetValue(accountId, out name);
        }
        return memberPhotos.TryGetValue(accountId, out (string Key, BitmapImage Image) photo)
            ? new MemberLook(photo.Image, $"{photo.Key}|{name}", name)
            : new MemberLook(null, $"|{name}", name);
    }
}
