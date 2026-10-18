using System.Collections.Generic;
using System.Linq;
using Omni.Windows.App.Features.Groups;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class GroupPeoplePickerModelTests
{
    private static GroupPickCandidate C(string title, GroupPickState state = GroupPickState.Available)
        => new() { DialogKey = "dialog:" + title.ToLowerInvariant(), Title = title, State = state };

    // Members and already-invited contacts stay visible (with the reason) but can't be picked again;
    // the pickable ones come first.
    [Fact]
    public void MembersAndInvitedAreListedLastAndCannotBePicked()
    {
        GroupPickCandidate carol = C("Carol", GroupPickState.Member);
        GroupPickCandidate dave = C("Dave", GroupPickState.Invited);
        GroupPickCandidate bob = C("Bob");
        GroupPeoplePickerModel model = new(new[] { carol, dave, bob });

        Assert.Equal(new[] { "Bob", "Dave", "Carol" }, model.Visible.Select(c => c.Title));
        Assert.False(model.Toggle(carol));
        Assert.False(model.Toggle(dave));
        Assert.Empty(model.Selected);
        Assert.Equal("уже в группе", carol.StateText);

        Assert.True(model.Toggle(bob));
        Assert.Equal(new[] { "dialog:bob" }, model.SelectedDialogKeys);
    }

    // Negative half: a second tap unpicks, and the chips follow pick order.
    [Fact]
    public void TogglingTwiceUnpicksAndChipsKeepPickOrder()
    {
        GroupPickCandidate anna = C("Anna");
        GroupPickCandidate bob = C("Bob");
        GroupPeoplePickerModel model = new(new[] { anna, bob });
        model.Toggle(bob);
        model.Toggle(anna);
        Assert.Equal(new[] { "Bob", "Anna" }, model.Selected.Select(c => c.Title));

        model.Toggle(bob);
        Assert.False(bob.IsSelected);
        Assert.Equal(new[] { "Anna" }, model.Selected.Select(c => c.Title));
    }

    [Fact]
    public void SearchFiltersByNameIgnoringCaseAndKeepsTheSelection()
    {
        GroupPickCandidate anna = C("Анна");
        GroupPickCandidate bob = C("Bob");
        GroupPeoplePickerModel model = new(new[] { anna, bob });
        model.Toggle(bob);

        model.SetQuery("ан");
        Assert.Equal(new[] { "Анна" }, model.Visible.Select(c => c.Title));
        Assert.Equal(new[] { "Bob" }, model.Selected.Select(c => c.Title));

        model.SetQuery("zzz");
        Assert.Empty(model.Visible);
        model.SetQuery("  ");
        Assert.Equal(2, model.Visible.Count);
    }

    [Fact]
    public void StateComesFromMembershipFirstThenOurInvitations()
    {
        HashSet<string> members = new() { "acct-carol" };
        HashSet<string> invited = new() { "dialog:dave", "dialog:carol" };
        Assert.Equal(GroupPickState.Member, GroupPeoplePickerModel.StateFor("dialog:carol", "acct-carol", members, invited));
        Assert.Equal(GroupPickState.Invited, GroupPeoplePickerModel.StateFor("dialog:dave", "acct-dave", members, invited));
        Assert.Equal(GroupPickState.Available, GroupPeoplePickerModel.StateFor("dialog:bob", "acct-bob", members, invited));
        // An unknown account never matches a member.
        Assert.Equal(GroupPickState.Available, GroupPeoplePickerModel.StateFor("dialog:x", string.Empty, new HashSet<string> { string.Empty }, invited));
    }

    [Fact]
    public void ButtonsCountThePickedPeople()
    {
        Assert.Equal("Пригласить", GroupPeoplePickerModel.InviteButtonText(0));
        Assert.Equal("Пригласить (2)", GroupPeoplePickerModel.InviteButtonText(2));
        Assert.Equal("Пропустить", GroupPeoplePickerModel.NextButtonText(0));
        Assert.Equal("Далее (1)", GroupPeoplePickerModel.NextButtonText(1));
    }
}
