using Omni.Windows.App.Core;

namespace Omni.Windows.App.ViewModels;

internal enum AppShellMode
{
    Booting,
    Onboarding,
    Authenticated,
}

internal enum OnboardingStep
{
    Welcome,
    BootRecovery,
    ProfileChooser,
    UnlockSavedProfile,
    CreateAccount,
    RecoveryPhrase,
    ImportAccount,
    RestoreAccount,
}

internal sealed class OnboardingState
{
    public AppShellMode ShellMode { get; set; } = AppShellMode.Booting;

    public OnboardingStep Step { get; set; } = OnboardingStep.Welcome;

    public LocalAccountSnapshot? LocalAccount { get; set; }

    public LocalDeviceSnapshot? LocalDevice { get; set; }

    public RecoveryPhraseV2? PendingRecoveryPhraseV2 { get; set; }

    public string? PendingImportFileName { get; set; }

    public string? PendingImportFilePath { get; set; }

    public string? ActiveProfileId { get; set; }

    public string? SelectedProfileId { get; set; }

    public RecoveryInputSummary? RecoveryPreview { get; set; }
}
