using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Features.Chats;
using Omni.Windows.App.Features.Contacts;

namespace Omni.Windows.App;

public sealed partial class MainWindow
{
    private void WireSectionEvents()
    {
        AcceptIncomingCallButton.Click += OnAcceptIncomingCallClicked;
        AcceptIncomingCallMutedButton.Click += OnAcceptIncomingCallMutedClicked;
        DeclineIncomingCallButton.Click += OnDeclineIncomingCallClicked;
        WireOnboardingSectionEvents();
        WireChatsSectionEvents();
        WireContactsSectionEvents();
        WireSettingsSectionEvents();
        WireDiagnosticsSectionEvents();
    }

    private void WireOnboardingSectionEvents()
    {
        CreateAccountButton.Click += OnShowCreateAccountClicked;
        RestoreAccountButton.Click += OnShowRestoreAccountClicked;
        ImportAccountButton.Click += OnShowImportAccountClicked;
        RetryBootButton.Click += OnRetryBootClicked;
        BootRecoveryResetButton.Click += OnDeleteLocalDataAndRetryBootClicked;
        BootRecoveryRetryButton.Click += OnRetryBootClicked;
        SavedProfilesListView.SelectionChanged += OnSavedProfilesSelectionChanged;
        OpenSavedProfileButton.Click += OnOpenSavedProfileClicked;
        DeleteSavedProfileFromChooserButton.Click += OnDeleteSavedProfileFromChooserClicked;
        CreateNewProfileFromChooserButton.Click += OnShowCreateAccountClicked;
        RestoreProfileFromChooserButton.Click += OnShowRestoreAccountClicked;
        ImportProfileFromChooserButton.Click += OnShowImportAccountClicked;
        UnlockWithPasswordButton.Click += OnUnlockWithPasswordClicked;
        UnlockWithMnemonicButton.Click += OnUnlockWithMnemonicClicked;
        BackToChooserButton.Click += OnBackToChooserClicked;
        CreateAccountExecuteButton.Click += OnCreateAccountClicked;
        BackToWelcomeFromCreateButton.Click += OnBackToWelcomeClicked;
        ChooseImportAccountFileButton.Click += OnChooseImportAccountFileClicked;
        ImportAccountMnemonicTextBox.TextChanged += OnMessageInputTextChanged;
        ImportAccountExecuteButton.Click += OnImportAccountClicked;
        BackToWelcomeFromImportButton.Click += OnBackToWelcomeClicked;
        SaveRecoveryToFileButton.Click += OnSaveRecoveryClicked;
        RecoveryContinueButton.Click += OnRecoveryContinueClicked;
        RestoreAccountExecuteButton.Click += OnRestoreAccountClicked;
        BackToWelcomeFromRestoreButton.Click += OnBackToWelcomeClicked;
    }

    private void WireChatsSectionEvents()
    {
        ChatsListView.SelectionChanged += OnChatsSelectionChanged;
        ChatSearchBox.TextChanged += OnChatSearchTextChanged;
        MessagesListView.Loaded += OnMessagesListViewLoaded;
        MessagesHostBorder.SizeChanged += OnMessagesHostBorderSizeChanged;
        OpenContactsFromChatsButton.Click += OnOpenContactsFromChatsClicked;
        CallVideoFrameRateComboBox.SelectionChanged += OnCallVideoPreferenceSelectionChanged;
        CallVideoCodecComboBox.SelectionChanged += OnCallVideoPreferenceSelectionChanged;
        CallVideoBitrateComboBox.SelectionChanged += OnCallVideoBitrateSelectionChanged;
        CallVideoEncodeDeviceComboBox.SelectionChanged += OnCallVideoDeviceSelectionChanged;
        CallVideoDecodeDeviceComboBox.SelectionChanged += OnCallVideoDeviceSelectionChanged;
        StartCurrentChatCallButton.Click += OnStartCurrentChatCallClicked;
        StartCurrentChatRemoteControlButton.Click += OnStartCurrentChatRemoteControlClicked;
        ReconnectContactButton.Click += OnReconnectContactClicked;
        DeleteChatContactButton.Click += OnChatHeaderMenuClicked;
        ChatHeaderGrid.IdentityClicked += (_, _) => OpenCurrentChatContactProfile();
        // Every avatar showing a picture opens it as a photo, wherever it is (AvatarView).
        Features.Shared.AvatarView.PictureRequested += (path, title) => OpenAvatarViewer(path, title);
        ChatContactInfoCloseButton.Click += (_, _) => CloseChatContactInfo();
        AcceptCurrentCallButton.Click += OnAcceptIncomingCallClicked;
        AcceptCurrentCallMutedButton.Click += OnAcceptIncomingCallMutedClicked;
        DeclineCurrentCallButton.Click += OnDeclineIncomingCallClicked;
        ToggleCurrentCallMuteButton.Click += OnToggleCurrentCallMuteClicked;
        ToggleCurrentCallCameraButton.Click += OnToggleCurrentCallCameraClicked;
        ToggleCurrentCallCameraButton.ContextRequested += OnCurrentCallCameraQualityRequested;
        ToggleCurrentCallScreenShareButton.Click += OnToggleCurrentCallScreenShareClicked;
        MinimizeCallButton.Click += OnMinimizeCallClicked;
        ActiveCallBarReturnButton.Click += OnReturnToCallClicked;
        ActiveCallBarMuteButton.Click += OnToggleCurrentCallMuteClicked;
        ActiveCallBarHangUpButton.Click += OnHangUpCurrentCallClicked;
        ToggleCurrentCallScreenControlButton.Click += OnToggleCurrentCallScreenControlClicked;
        ToggleRemoteScreenControlButton.Click += OnToggleRemoteScreenControlClicked;
        ToggleCallFullScreenButton.Click += OnToggleCallFullScreenClicked;
        WireRemoteScreenControlSurface(RemoteScreenControlSurface);
        HangUpCurrentCallButton.Click += OnHangUpCurrentCallClicked;
        MessagesHostBorder.DragOver += OnChatMessagesDragOver;
        MessagesHostBorder.DragLeave += OnChatMessagesDragLeave;
        MessagesHostBorder.Drop += OnChatMessagesDrop;
        ChatComposerBorder.DraftRemoveRequested += (_, _) => OnRemoveDraftAttachmentClicked(ChatComposerBorder, new RoutedEventArgs());
        ChatComposerBorder.DraftAsPhotoChanged += (_, asPhoto) => pendingAttachmentAsPhoto = asPhoto;
        ChatsSectionHost.AttachmentPhotoClicked += OnAttachmentPhotoClicked;
        ChatsSectionHost.AttachmentPhotoSaveClicked += (sender, _) =>
        {
            if (sender is FrameworkElement { Tag: string messageId } &&
                TryGetCurrentChatMessage(messageId)?.Attachment is { LocalCopyPath: { Length: > 0 } path } attachment)
            {
                SaveViewedImageAs(path, attachment.FileName);
            }
            else
            {
                SetActionStatus("Фото ещё загружается.");
            }
        };
        AttachmentMenuButton.Click += OnAttachFilesButtonClicked;
        MessageInputTextBox.TextChanged += OnMessageInputTextChanged;
        // The button and Enter alike (Features/Shared/MessageComposer).
        ChatComposerBorder.SendRequested += (sender, _) => OnSendClicked(sender!, new RoutedEventArgs());
        ShowLocalPrekeyBundleButton.Click += OnShowLocalPrekeyBundleClicked;
        RemotePrekeyBundleTextBox.TextChanged += OnRemotePrekeyBundleTextChanged;
        ImportRemotePrekeyBundleButton.Click += OnImportRemotePrekeyBundleClicked;
        ChatsSectionHost.AttachmentCardClicked += OnAttachmentCardClicked;
        ChatsSectionHost.BackToListRequested += OnChatBackToListRequested;
        WireGroupConversation();
    }

    private void OnChatBackToListRequested(object? sender, EventArgs e)
    {
        // Back from a group closes only the group view — a running group call keeps going.
        if (currentGroupRowKey is not null)
        {
            CloseGroupConversation(clearSelection: true);
            return;
        }

        CloseCurrentChatSelection();
    }

    private void WireContactsSectionEvents()
    {
        ContactsAddToggleButton.Click += OnContactsAddToggleClicked;
        ContactsFilterAllButton.Click += OnContactsFilterModeClicked;
        ContactsFilterPendingButton.Click += OnContactsFilterModeClicked;
        ContactsFilterApprovedButton.Click += OnContactsFilterModeClicked;
        ContactsFilterDeclinedButton.Click += OnContactsFilterModeClicked;
        FilteredContactsListView.SelectionChanged += OnContactsSelectionChanged;
        GenerateMeshInviteButton.Click += OnGenerateMeshInviteClicked;
        PairOverMeshButton.Click += OnPairOverMeshClicked;
        GenerateReachTokenButton.Click += OnGenerateReachTokenClicked;
        DialReachTokenButton.Click += OnDialReachTokenClicked;
        CopyMeshInviteButton.Click += OnCopyMeshInviteClicked;
        CopyReachTokenButton.Click += OnCopyReachTokenClicked;
        QuickAddContactPetnameTextBox.TextChanged += OnMessageInputTextChanged;
        QuickAddContactButton.Click += OnQuickAddContactClicked;
        AddContactByIdentifierButton.Click += OnAddContactByIdentifierClicked;
        AddContactFromPayloadButton.Click += OnAddContactFromPayloadClicked;
        AddByOmniIdButton.Click += OnAddByOmniIdClicked;
        FindPeopleMoreButton.Click += OnFindPeopleMoreClicked;
        // Enter in the field is "Найти".
        AddByOmniIdTextBox.KeyDown += (_, args) =>
        {
            if (args.Key == global::Windows.System.VirtualKey.Enter && AddByOmniIdButton.IsEnabled)
            {
                args.Handled = true;
                OnAddByOmniIdClicked(AddByOmniIdButton, new Microsoft.UI.Xaml.RoutedEventArgs());
            }
        };
        OpenContactChatButton.Click += OnOpenContactChatClicked;
        StartSelectedContactCallButton.Click += OnStartSelectedContactCallClicked;
        StartSelectedContactRemoteControlButton.Click += OnStartSelectedContactRemoteControlClicked;
        ContactActionsButton.Click += OnContactActionsClicked;
        RenameContactButton.Click += OnRenameContactClicked;
        ConfirmVerificationButton.Click += OnConfirmContactVerificationClicked;
        ToggleManualVerificationButton.Click += (_, _) =>
            ManualVerificationPanel.Visibility = ManualVerificationPanel.Visibility == Visibility.Visible
                ? Visibility.Collapsed
                : Visibility.Visible;
        ContactRemoteAccessToggle.Toggled += OnContactRemoteAccessToggled;
        ContactRemoteAccessAskRadio.Checked += OnContactRemoteAccessModeChanged;
        ContactRemoteAccessUnattendedRadio.Checked += OnContactRemoteAccessModeChanged;
    }

    private void WireSettingsSectionEvents()
    {
        SettingsPublicDisplayNameTextBox.TextChanged += OnMessageInputTextChanged;
        SettingsChooseAvatarButton.Click += OnSettingsChooseAvatarClicked;
        SettingsClearAvatarButton.Click += OnSettingsClearAvatarClicked;
        SettingsSavePublicProfileButton.Click += OnSettingsSavePublicProfileClicked;
        SettingsSignOutButton.Click += OnSettingsSignOutClicked;
        SettingsDeleteLocalProfileButton.Click += OnSettingsDeleteLocalProfileClicked;
        SettingsCameraComboBox.SelectionChanged += OnSettingsCameraSelectionChanged;
        SettingsMicrophoneComboBox.SelectionChanged += OnSettingsAudioDeviceSelectionChanged;
        SettingsSpeakerComboBox.SelectionChanged += OnSettingsAudioDeviceSelectionChanged;
        SettingsThemeComboBox.SelectionChanged += OnSettingsThemeSelectionChanged;
        SettingsCongestionComboBox.SelectionChanged += OnSettingsCongestionSelectionChanged;
        SettingsRevealRecoveryButton.Click += OnRevealRecoveryClicked;
        SettingsSaveRecoveryButton.Click += OnSaveRecoveryClicked;
        SettingsExportAccountButton.Click += OnExportAccountClicked;
        SettingsRevokeAllRemoteAccessButton.Click += OnRevokeAllRemoteAccessClicked;
        SettingsSectionHost.RevokeDeviceRequested += (_, deviceId) => RevokeOwnDevice(deviceId);
        SettingsMoveAccountKeyButton.Click += OnMoveAccountKeyClicked;
        SettingsAcknowledgeDevicesButton.Click += OnAcknowledgeOwnDevicesClicked;
        SettingsHideAllRevokedDevicesButton.Click += OnHideAllRevokedDevicesClicked;
        SettingsSectionHost.HideRevokedDeviceRequested += OnHideRevokedDeviceRequested;
        SettingsRunInBackgroundToggle.Toggled += OnRunInBackgroundToggled;
        SettingsSetPasswordButton.Click += OnSettingsSetPasswordClicked;
        SettingsChangePasswordButton.Click += OnSettingsChangePasswordClicked;
        SettingsRemovePasswordButton.Click += OnSettingsRemovePasswordClicked;
        SettingsCopyOmniIdButton.Click += OnCopyOmniIdClicked;
        SettingsDiscoverabilityToggle.Toggled += OnDiscoverabilityToggled;
        SettingsNameFindableToggle.Toggled += OnNameFindableToggled;
    }

    private void WireDiagnosticsSectionEvents()
    {
        DiagnosticsOpenIsolatedWindowButton.Click += OnOpenIsolatedWindowClicked;
        DiagnosticsPingButton.Click += OnPingClicked;
    }
}
