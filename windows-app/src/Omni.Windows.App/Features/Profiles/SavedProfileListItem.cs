namespace Omni.Windows.App.Features.Profiles;

internal sealed record SavedProfileListItem(
    string ProfileId,
    string DisplayName,
    string AccountId,
    string State,
    string StateLabel);
