using Omni.Windows.App.Core;

namespace Omni.Windows.App.Presentation;

internal static partial class MainWindowPresentation
{
    internal const double TranscriptBubbleMaxWidthHardCap = 620d;
    private const double TranscriptBubbleWidthFactor = 0.72d;

    public static string BuildIdentitySubtitle(LocalAccountSnapshot? account, LocalDeviceSnapshot? device)
    {
        // Compact account-chip label for the nav-rail footer: just the profile name (the device
        // label + details live in Settings) — like the account row other messengers put at the
        // bottom of their navigation, not a full-width app-name banner.
        _ = device;
        return account is null ? "Профиль не загружен" : account.Descriptor.DisplayName;
    }
}
