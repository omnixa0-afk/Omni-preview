using Omni.Windows.App.ViewModels;

namespace Omni.Windows.App;

internal sealed class StartupPolicyService
{
    public bool CanRunAuthenticatedRefresh(
        AppShellMode shellMode,
        bool isInitialized,
        bool isConnecting)
        => shellMode == AppShellMode.Authenticated &&
           isInitialized &&
           !isConnecting;
}
