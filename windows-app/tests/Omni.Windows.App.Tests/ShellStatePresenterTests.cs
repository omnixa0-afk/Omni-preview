using Microsoft.UI.Xaml;
using Omni.Windows.App.Presentation;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class ShellStatePresenterTests
{
    [Fact]
    public void Build_DisablesSend_WhenNoApprovedLinkedChatContact()
    {
        ShellControlStateInput input = CreateDefaultInput() with
        {
            HasCurrentDialog = true,
            HasMessageText = true,
            HasApprovedLinkedChatContact = false,
        };

        ShellControlState state = ShellStatePresenter.Build(input);

        Assert.False(state.IsSendEnabled);
        Assert.True(state.IsAttachmentMenuEnabled);
        Assert.False(state.IsMessageInputEnabled);
    }

    [Fact]
    public void Build_EnablesRecoveryActions_OnlyWhenRecoveryAccessIsPasswordProtected()
    {
        ShellControlState disabledState = ShellStatePresenter.Build(CreateDefaultInput() with
        {
            LifecycleUnlocked = true,
            HasCurrentProfileId = true,
            HasPasswordProtectedRecoveryAccess = false,
        });

        ShellControlState enabledState = ShellStatePresenter.Build(CreateDefaultInput() with
        {
            LifecycleUnlocked = true,
            HasCurrentProfileId = true,
            HasPasswordProtectedRecoveryAccess = true,
        });

        Assert.False(disabledState.IsSettingsRevealRecoveryEnabled);
        Assert.False(disabledState.IsSettingsSaveRecoveryEnabled);
        Assert.True(enabledState.IsSettingsRevealRecoveryEnabled);
        Assert.True(enabledState.IsSettingsSaveRecoveryEnabled);
    }

    [Fact]
    public void Build_KeepsOnboardingControlsDisabled_WhileOnboardingIsBusy()
    {
        ShellControlState state = ShellStatePresenter.Build(CreateDefaultInput() with
        {
            IsInitialized = false,
            IsAuthenticated = false,
            IsOnboarding = true,
            IsOnboardingBusy = true,
            HasPendingImportFilePath = true,
            HasImportAccountMnemonicText = true,
            HasSelectedOnboardingProfile = true,
            LifecycleRequiresUnlock = true,
        });

        Assert.False(state.IsCreateDisplayNameEnabled);
        Assert.False(state.IsImportAccountExecuteEnabled);
        Assert.False(state.IsOpenSavedProfileEnabled);
        Assert.False(state.IsUnlockWithPasswordEnabled);
        Assert.False(state.IsBootRecoveryRetryEnabled);
    }

    [Fact]
    public void Build_HidesContactSpecificControls_WhenNoContactIsSelected()
    {
        ShellControlState state = ShellStatePresenter.Build(CreateDefaultInput());

        Assert.False(state.IsContactActionsEnabled);
        Assert.Equal(Visibility.Collapsed, state.ContactActionsVisibility);
        Assert.False(state.IsRenameContactEnabled);
        Assert.Equal(Visibility.Collapsed, state.RenameContactVisibility);
        Assert.False(state.IsOpenContactChatEnabled);
    }

    [Fact]
    public void Build_EnablesDiagnosticsPing_WhenTransportIsAvailable()
    {
        ShellControlState state = ShellStatePresenter.Build(CreateDefaultInput());

        Assert.True(state.IsPingEnabled);
    }

    [Fact]
    public void Build_DisablesDiagnosticsPing_WhenTransportIsBlocked()
    {
        ShellControlState state = ShellStatePresenter.Build(CreateDefaultInput() with
        {
            LifecycleTransportAllowed = false,
        });

        Assert.False(state.IsPingEnabled);
    }

    private static ShellControlStateInput CreateDefaultInput()
        => new(
            IsInitialized: true,
            IsAuthenticated: true,
            LifecycleMessagingAllowed: true,
            LifecycleTransportAllowed: true,
            LifecycleDestructiveAllowed: true,
            LifecycleDestructiveInProgress: false,
            LifecycleUnlocked: false,
            LifecycleRequiresUnlock: false,
            NodeModeAllowsNetwork: true,
            HasMessageText: false,
            HasImportAccountMnemonicText: false,
            HasDraftAttachment: false,
            HasCurrentDialog: false,
            HasLinkedChatContact: false,
            HasApprovedLinkedChatContact: false,
            HasSelectedContact: false,
            HasApprovedSelectedContact: false,
            HasRunnableSelectedContactChat: false,
            IsConnecting: false,
            HasCurrentProfile: true,
            HasCurrentOrPendingPublicAvatar: false,
            HasPendingPublicProfileChanges: false,
            HasPasswordProtectedRecoveryAccess: false,
            HasCurrentProfileId: false,
            IsUpdatingPublicProfile: false,
            HasQuickAddContactPetname: false,
            HasRemotePrekeyBundle: false,
            HasAddContactIdentifier: false,
            HasAddContactPetname: false,
            HasAddContactPayload: false,
            IsOnboarding: false,
            IsOnboardingBusy: false,
            HasPendingImportFilePath: false,
            HasSelectedOnboardingProfile: false);
}
