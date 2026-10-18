using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Media.Imaging;

namespace Omni.Windows.App.Features.Contacts;

public sealed record ContactListItem(
    string ContactId,
    string RelationshipState,
    string? PendingRequestDirection,
    bool HasMet,
    string AvatarText,
    BitmapImage? AvatarImage,
    Visibility AvatarImageVisibility,
    Visibility AvatarFallbackVisibility,
    Visibility AvatarLoadingVisibility,
    string Title,
    string StatusText,
    string BadgeText,
    // Presence dot on the avatar: Visible (green = online / amber = recently online) or
    // Collapsed (offline / no presence). PresenceDotBrush is the dot fill.
    Visibility PresenceDotVisibility,
    Brush PresenceDotBrush,
    string Signature,
    // What picks the photo-less avatar's colour: the account id (AvatarPalette).
    string AvatarSeed = "")
{
    // Read by the binding on the UI thread only.
    public Brush AvatarBrush => Omni.Windows.App.Presentation.AvatarPalette.BrushFor(AvatarSeed);

    // The shared row (App.xaml, PersonRowTemplate) reads a chat row's names: the status is the
    // second line, the badge the corner word; a contact has no time and no group mark.
    public string PreviewText => StatusText;
    public string CornerText => BadgeText;
    public string TimeText => string.Empty;
    public Visibility GroupMarkVisibility => Visibility.Collapsed;
    // No unread counter on a contact row (a missing binding source would leave it Visible).
    public Visibility UnreadBadgeVisibility => Visibility.Collapsed;
    public string UnreadBadgeText => string.Empty;
}
