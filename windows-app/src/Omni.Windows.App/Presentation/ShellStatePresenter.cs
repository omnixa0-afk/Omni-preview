using Microsoft.UI.Xaml;

namespace Omni.Windows.App.Presentation;

internal static class ShellStatePresenter
{
    public static ShellControlState Build(ShellControlStateInput input)
    {
        bool onboardingEnabled = input.IsOnboarding && !input.IsOnboardingBusy;

        return new ShellControlState(
            IsPingEnabled:
                input.IsInitialized && input.IsAuthenticated && input.LifecycleTransportAllowed &&
                input.NodeModeAllowsNetwork && !input.IsConnecting,
            IsSendEnabled:
                input.IsInitialized && input.IsAuthenticated && input.LifecycleMessagingAllowed &&
                input.NodeModeAllowsNetwork && (input.HasMessageText || input.HasDraftAttachment) &&
                input.HasCurrentDialog && input.HasApprovedLinkedChatContact && !input.IsConnecting,
            IsAttachmentMenuEnabled:
                input.IsInitialized && input.IsAuthenticated && input.LifecycleMessagingAllowed &&
                input.NodeModeAllowsNetwork && input.HasCurrentDialog && !input.IsConnecting,
            IsMessageInputEnabled:
                input.IsInitialized && input.IsAuthenticated && input.LifecycleMessagingAllowed &&
                input.NodeModeAllowsNetwork && input.HasCurrentDialog && input.HasApprovedLinkedChatContact &&
                !input.IsConnecting,
            IsDeleteChatContactEnabled:
                input.IsInitialized && input.IsAuthenticated && input.LifecycleMessagingAllowed &&
                input.HasLinkedChatContact && !input.IsConnecting,
            DeleteChatContactVisibility: input.HasLinkedChatContact ? Visibility.Visible : Visibility.Collapsed,
            IsQuickAddContactEnabled:
                input.IsInitialized && input.IsAuthenticated && input.LifecycleMessagingAllowed &&
                input.HasCurrentDialog && input.HasQuickAddContactPetname && !input.IsConnecting,
            IsShowLocalPrekeyBundleEnabled:
                input.IsInitialized && input.IsAuthenticated && input.LifecycleMessagingAllowed,
            IsImportRemotePrekeyBundleEnabled:
                input.IsInitialized && input.IsAuthenticated && input.LifecycleMessagingAllowed &&
                input.HasRemotePrekeyBundle,
            IsAddContactByIdentifierEnabled:
                input.IsInitialized && input.IsAuthenticated && input.LifecycleMessagingAllowed &&
                input.HasAddContactIdentifier && input.HasAddContactPetname,
            IsAddContactFromPayloadEnabled:
                input.IsInitialized && input.IsAuthenticated && input.LifecycleMessagingAllowed &&
                input.HasAddContactPayload && input.HasAddContactPetname,
            IsRenameContactEnabled:
                input.IsInitialized && input.IsAuthenticated && input.LifecycleMessagingAllowed &&
                input.HasSelectedContact,
            RenameContactVisibility: input.HasSelectedContact ? Visibility.Visible : Visibility.Collapsed,
            IsContactActionsEnabled:
                input.IsInitialized && input.IsAuthenticated && input.LifecycleMessagingAllowed &&
                input.HasSelectedContact,
            ContactActionsVisibility: input.HasSelectedContact ? Visibility.Visible : Visibility.Collapsed,
            IsOpenContactChatEnabled:
                input.IsInitialized && input.IsAuthenticated && input.LifecycleMessagingAllowed &&
                input.HasRunnableSelectedContactChat,
            OpenContactChatVisibility:
                input.HasApprovedSelectedContact ? Visibility.Visible : Visibility.Collapsed,
            IsConfirmVerificationEnabled:
                input.IsInitialized && input.IsAuthenticated && input.LifecycleMessagingAllowed &&
                input.HasApprovedSelectedContact,
            ConfirmVerificationVisibility:
                input.HasApprovedSelectedContact ? Visibility.Visible : Visibility.Collapsed,
            IsSettingsRevealRecoveryEnabled:
                input.IsInitialized && input.IsAuthenticated && input.LifecycleUnlocked &&
                input.HasPasswordProtectedRecoveryAccess && input.HasCurrentProfileId,
            IsSettingsSaveRecoveryEnabled:
                input.IsInitialized && input.IsAuthenticated && input.LifecycleUnlocked &&
                input.HasPasswordProtectedRecoveryAccess && input.HasCurrentProfileId,
            IsOpenIsolatedWindowEnabled:
                input.IsInitialized && input.IsAuthenticated,
            IsSettingsSignOutEnabled:
                input.IsInitialized && input.IsAuthenticated && input.LifecycleUnlocked &&
                !input.LifecycleDestructiveInProgress,
            IsSettingsDeleteLocalProfileEnabled:
                input.IsInitialized && input.IsAuthenticated && input.LifecycleDestructiveAllowed &&
                !input.LifecycleDestructiveInProgress,
            IsSettingsPublicDisplayNameEnabled:
                input.IsInitialized && input.IsAuthenticated && input.LifecycleUnlocked &&
                !input.LifecycleDestructiveInProgress && !input.IsUpdatingPublicProfile,
            IsSettingsChooseAvatarEnabled:
                input.IsInitialized && input.IsAuthenticated && input.LifecycleUnlocked &&
                input.HasCurrentProfile && !input.LifecycleDestructiveInProgress &&
                !input.IsUpdatingPublicProfile,
            IsSettingsClearAvatarEnabled:
                input.IsInitialized && input.IsAuthenticated && input.LifecycleUnlocked &&
                input.HasCurrentOrPendingPublicAvatar && !input.LifecycleDestructiveInProgress &&
                !input.IsUpdatingPublicProfile,
            IsSettingsSavePublicProfileEnabled:
                input.IsInitialized && input.IsAuthenticated && input.LifecycleUnlocked &&
                input.HasCurrentProfile && input.HasPendingPublicProfileChanges &&
                !input.LifecycleDestructiveInProgress && !input.IsUpdatingPublicProfile,
            IsSettingsSetPasswordEnabled:
                input.IsInitialized && input.IsAuthenticated && input.HasCurrentProfileId &&
                !input.LifecycleDestructiveInProgress,
            IsSettingsChangePasswordEnabled:
                input.IsInitialized && input.IsAuthenticated && input.HasCurrentProfileId &&
                !input.LifecycleDestructiveInProgress,
            IsSettingsRemovePasswordEnabled:
                input.IsInitialized && input.IsAuthenticated && input.HasCurrentProfileId &&
                !input.LifecycleDestructiveInProgress,
            IsCreateDisplayNameEnabled: onboardingEnabled,
            IsCreateDeviceLabelEnabled: onboardingEnabled,
            IsBootRecoveryResetEnabled: onboardingEnabled,
            IsBootRecoveryRetryEnabled: onboardingEnabled,
            IsRestoreDisplayNameEnabled: onboardingEnabled,
            IsRestoreRecoveryPhraseEnabled: onboardingEnabled,
            IsRestoreDeviceLabelEnabled: onboardingEnabled,
            IsImportAccountMnemonicEnabled: onboardingEnabled && input.HasPendingImportFilePath,
            IsImportAccountExecuteEnabled:
                onboardingEnabled && input.HasPendingImportFilePath && input.HasImportAccountMnemonicText,
            IsOpenSavedProfileEnabled: onboardingEnabled && input.HasSelectedOnboardingProfile,
            IsDeleteSavedProfileFromChooserEnabled: onboardingEnabled && input.HasSelectedOnboardingProfile,
            IsUnlockWithPasswordEnabled: onboardingEnabled && input.LifecycleRequiresUnlock,
            IsUnlockWithMnemonicEnabled: onboardingEnabled && input.LifecycleRequiresUnlock,
            IsSettingsExportAccountEnabled:
                input.IsInitialized && input.IsAuthenticated && input.LifecycleUnlocked);
    }
}
