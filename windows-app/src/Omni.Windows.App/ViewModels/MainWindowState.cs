using Omni.Windows.App.Core;

namespace Omni.Windows.App.ViewModels;

internal sealed class MainWindowState
{
    public OnboardingState Onboarding { get; } = new();

    public AuthenticatedShellSection CurrentSection { get; set; } = AuthenticatedShellSection.Chats;

    public string? CurrentPeerKey { get; set; }

    public string? CurrentTransportPeerKey { get; set; }

    public string? SelectedContactId { get; set; }

    public long? SelectedContactRevision { get; set; }

    public string LastRenderedMessageSignature { get; set; } = string.Empty;

    public string LastRenderedChatSignature { get; set; } = string.Empty;

    public string LastObservedChatRefreshSignature { get; set; } = string.Empty;

    public string LastRenderedContactSignature { get; set; } = string.Empty;

    public string LastObservedContactRefreshSignature { get; set; } = string.Empty;

    public bool IsConnecting { get; set; }


    public string? SelectedSignalingStunServer { get; set; }
}
