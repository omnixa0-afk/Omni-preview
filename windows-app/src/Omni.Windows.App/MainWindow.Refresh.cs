using Microsoft.UI.Xaml;
using Omni.Windows.App.Core;
using Omni.Windows.App.Presentation;
using Omni.Windows.App.ViewModels;

namespace Omni.Windows.App;

public sealed partial class MainWindow
{
    private void RefreshQueueDiagnostics()
    {
        if (!rustBridge.IsInitialized || windowState.Onboarding.ShellMode != AppShellMode.Authenticated)
        {
            QueueSummaryTextBlock.Text = "Очередь сообщений недоступна.";
            QueueDiagnosticsTextBlock.Text = "Очередь сообщений недоступна.";
            SecurityDiagnosticsTextBlock.Text = "Сводка безопасности для текущего диалога недоступна.";
            lastQueueDiagnosticsSnapshot = null;
            return;
        }

        ApplyQueueDiagnosticsRefreshPayload(new QueueDiagnosticsRefreshPayload(
            lastQueueDiagnosticsSnapshot ?? rustBridge.GetQueueDiagnostics(),
            GetResolvedCurrentDialogKey()));
    }

    private void ApplyQueueDiagnosticsRefreshPayload(QueueDiagnosticsRefreshPayload payload)
    {
        lastQueueDiagnosticsSnapshot = payload.Diagnostics;
        string summary = MainWindowPresentation.BuildQueueDiagnostics(
            payload.Diagnostics,
            payload.DialogKey);
        QueueSummaryTextBlock.Text = summary;
        QueueDiagnosticsTextBlock.Text = summary;
    }

    private void EnterAuthenticatedShell(string actionStatus)
    {
        windowState.Onboarding.ShellMode = AppShellMode.Authenticated;
        SetShellMode(AppShellMode.Authenticated);
        ClearDangerousActionPreview();
        RefreshCurrentLifecycleContracts();
        UpdateIdentityHeader();
        ResetPrekeyExchangeUi();
        SetActionStatus(actionStatus);
        DiagnosticsPingButton.IsEnabled = true;

        RefreshTransportStatus();
        RefreshQueueDiagnostics();
        RefreshCallInbox();
        RefreshChatHeaderSummary();
        RefreshMessages(force: true);
        RefreshSelectedContactCard();
        DateTimeOffset now = DateTimeOffset.UtcNow;
        nextLifecycleRefreshAt = now + LifecycleRefreshCadence;
        nextTransportRefreshAt = now + TransportRefreshCadence;
        nextCallRefreshAt = now + GetCallRefreshCadence();
        nextChatHeaderRefreshAt = now + ChatHeaderRefreshCadence;
        nextChatListRefreshAt = now + ChatListRefreshCadence;
        nextMessageRefreshAt = now + MessageRefreshCadence;
        nextQueueRefreshAt = now + QueueRefreshCadence;
        nextContactsRefreshAt = now + ContactsRefreshCadence;
        nextSettingsRefreshAt = now + SettingsRefreshCadence;
        nextFootprintRefreshAt = now + FootprintRefreshCadence;
        windowState.LastObservedChatRefreshSignature = string.Empty;
        windowState.LastObservedContactRefreshSignature = string.Empty;
        UpdateControlStates();
        StartAuthenticatedRefreshLoop();
        TryStartMeshOverlay();
        // The two HEAVY full-list FFI reads (contacts + chats) used to run synchronously RIGHT HERE on
        // the UI thread, at the exact moment TryStartMeshOverlay's background StartMesh writes the DB —
        // so each blocked on the 5 s WAL busy timeout under that write contention and FROZE the boot
        // shell (couldn't switch tabs / laggy — live-observed 2026-07-24). Read them OFF the UI thread and
        // apply on the UI thread, mirroring StartMesh + the app's existing `await Task.Run(rustBridge.Get…)`
        // pattern. Messages already load async via RefreshMessages→StartTailRebaseTranscriptLoad.
        LoadContactsListAsync(windowState.SelectedContactId);
        LoadChatsListAsync(windowState.CurrentPeerKey);
    }

    /// Read the heavy contacts list snapshot OFF the UI thread (the FFI opens SQLite + queries the full
    /// list), then apply it on the UI thread. Used on the boot path AND on a tab switch to Contacts, so
    /// neither freezes the UI on a blocking read under background write contention (the tab flips visible
    /// instantly; the list fills in a moment later). `async void` is fire-and-forget like the app's other
    /// one-shot handlers; the await is guarded so nothing escapes. The periodic refresh loop already
    /// backgrounds its own reads (`FetchBackgroundRefreshAsync`); the after-user-action call sites stay
    /// synchronous (warm reads right after an FFI mutation).
    private async void LoadContactsListAsync(string? selectContactId)
    {
        try
        {
            ContactsScreenSnapshot contacts = await System.Threading.Tasks.Task.Run(
                () => rustBridge.GetContactsScreenSnapshot());
            if (windowState.Onboarding.ShellMode == AppShellMode.Authenticated)
            {
                windowState.LastObservedContactRefreshSignature =
                    BuildContactsRefreshSignature(contacts.RefreshSummary);
                ApplyContactsSnapshots(contacts.Contacts, force: true, selectContactId);
            }
        }
        catch (Exception ex)
        {
            AppendNavRefreshLog("async.contacts_list", ex);
        }
    }

    /// Read the heavy chats list snapshot OFF the UI thread, then apply it on the UI thread. Same rationale
    /// as <see cref="LoadContactsListAsync"/> — boot + tab switch to Chats without a UI-thread freeze.
    private async void LoadChatsListAsync(string? selectDialogKey)
    {
        try
        {
            (IReadOnlyList<RustBridge.ChatThreadProjection> projections, ChatGroupRows groupRows) =
                await System.Threading.Tasks.Task.Run(() =>
                    ((IReadOnlyList<RustBridge.ChatThreadProjection>)rustBridge.ListChatThreadProjections(),
                     ChatGroupRows.Fetch(rustBridge)));
            if (windowState.Onboarding.ShellMode == AppShellMode.Authenticated)
            {
                ApplyChatThreadProjections(projections, force: true, selectDialogKey, groupRows: groupRows);
            }
        }
        catch (Exception ex)
        {
            AppendNavRefreshLog("async.chats_list", ex);
        }
    }

    /// <summary>
    /// Best-effort: bring up the mesh overlay for the just-activated profile.
    /// The mesh announces + accepts and its auto-connect worker dials approved
    /// contacts, so chat and calls can ride the verified mesh Link without an
    /// <c>om3</c> signaling token. The mesh is an additive transport — the
    /// legacy path still works — so a failure here must never block sign-in;
    /// we swallow the error and leave the session on the legacy transport.
    /// </summary>
    private void TryStartMeshOverlay()
    {
        // StartMesh binds sockets + gathers reflexive candidates (STUN/UPnP/NAT-PMP) and can take
        // several seconds; running it on the UI thread froze the whole sign-in. Run it in the
        // background — mesh is additive, nothing on the UI thread awaits it, and the refresh loop
        // picks up the resulting transport state. (Was synchronous → ~10 s UI freeze, see
        // %TEMP%/omni-boot-timing.log.)
        _ = System.Threading.Tasks.Task.Run(() =>
        {
            try
            {
                // Выбор контроллера читается при СОЗДАНИИ эндпоинта, а на старте окна ядро часто
                // ещё холодное — поэтому применяем здесь, где точно успеваем до сокетов.
                ApplyCongestionModeToCore(LoadPersistedCongestionMode());
                rustBridge.StartMesh();
            }
            catch (Exception ex)
            {
                System.Diagnostics.Debug.WriteLine(
                    $"StartMesh failed; continuing on legacy transport: {ex.Message}");
            }

            DispatcherQueue.TryEnqueue(RefreshTransportStatus);
        });
    }

    /// <summary>
    /// Best-effort teardown of the mesh overlay on sign-out / shell exit.
    /// Idempotent on the Rust side (no-op when the mesh was never started), so
    /// it is safe to call unconditionally when leaving the authenticated shell.
    /// </summary>
    private void TryStopMeshOverlay()
    {
        try
        {
            rustBridge.StopMesh();
        }
        catch (Exception ex)
        {
            System.Diagnostics.Debug.WriteLine($"StopMesh failed: {ex.Message}");
        }
    }

    private void SetShellMode(AppShellMode mode)
    {
        BootRoot.Visibility = mode == AppShellMode.Booting ? Visibility.Visible : Visibility.Collapsed;
        OnboardingRoot.Visibility = mode == AppShellMode.Onboarding ? Visibility.Visible : Visibility.Collapsed;
        AuthenticatedRoot.Visibility = mode == AppShellMode.Authenticated ? Visibility.Visible : Visibility.Collapsed;

        // Select the Chats rail item on entering the shell so the nav shows its accent indicator
        // (we default to the Chats section; without a SelectedItem the rail looks like nothing is
        // active).
        if (mode == AppShellMode.Authenticated && AuthenticatedNavigationView.SelectedItem is null)
        {
            AuthenticatedNavigationView.SelectedItem = ChatsNavigationItem;
        }
    }

    private void SetLaunchWindowStatus(string message)
    {
        if (windowState.Onboarding.ShellMode == AppShellMode.Authenticated)
        {
            DiagnosticsToolsStatusTextBlock.Text = message;
            return;
        }

        OnboardingStatusTextBlock.Text = message;
    }

    private void UpdateControlStates()
    {
        bool isInitialized = rustBridge.IsInitialized;
        bool isAuthenticated = windowState.Onboarding.ShellMode == AppShellMode.Authenticated;
        bool lifecycleMessagingAllowed = currentProfileLifecycleSummary?.MessagingAllowed ?? false;
        bool lifecycleTransportAllowed = currentProfileLifecycleSummary?.TransportAllowed ?? false;
        bool lifecycleDestructiveAllowed = currentProfileLifecycleSummary?.DestructiveActionAllowed ?? false;
        bool lifecycleDestructiveInProgress = currentProfileLifecycleSummary?.DestructiveTransitionInProgress ?? false;
        bool lifecycleUnlocked = currentProfileLifecycleSummary?.IsUnlocked ?? false;
        bool lifecycleRequiresUnlock = currentProfileLifecycleSummary?.RequiresUnlock ?? false;
        bool nodeModeAllowsNetwork = lifecycleTransportAllowed;
        bool hasCurrentProfileId = !string.IsNullOrWhiteSpace(GetCurrentProfileId());
        bool hasSelectedContact = !string.IsNullOrWhiteSpace(windowState.SelectedContactId);
        bool hasApprovedSelectedContact =
            selectedContactCard is not null &&
            string.Equals(selectedContactCard.Contact.RelationshipState, "approved", StringComparison.OrdinalIgnoreCase);
        bool hasRunnableSelectedContactChat =
            hasApprovedSelectedContact &&
            selectedContactCard is not null &&
            (!string.IsNullOrWhiteSpace(selectedContactCard.Contact.PrimaryDialogKey) ||
             !string.IsNullOrWhiteSpace(selectedContactCard.Contact.RemoteInstanceId) ||
             !string.IsNullOrWhiteSpace(selectedContactCard.Contact.RemoteAccountId));
        bool hasApprovedLinkedChatContact = CanComposeInCurrentChat();

        ShellControlState controlState = ShellStatePresenter.Build(new ShellControlStateInput(
            IsInitialized: isInitialized,
            IsAuthenticated: isAuthenticated,
            LifecycleMessagingAllowed: lifecycleMessagingAllowed,
            LifecycleTransportAllowed: lifecycleTransportAllowed,
            LifecycleDestructiveAllowed: lifecycleDestructiveAllowed,
            LifecycleDestructiveInProgress: lifecycleDestructiveInProgress,
            LifecycleUnlocked: lifecycleUnlocked,
            LifecycleRequiresUnlock: lifecycleRequiresUnlock,
            NodeModeAllowsNetwork: nodeModeAllowsNetwork,
            HasMessageText: !string.IsNullOrWhiteSpace(MessageInputTextBox.Text),
            HasImportAccountMnemonicText: !string.IsNullOrWhiteSpace(ImportAccountMnemonicTextBox.Text),
            HasDraftAttachment: HasPendingAttachmentDraft(),
            HasCurrentDialog: !string.IsNullOrWhiteSpace(GetResolvedCurrentDialogKey()),
            HasLinkedChatContact: currentChatHeaderSummary?.ContactCard is not null,
            HasApprovedLinkedChatContact: hasApprovedLinkedChatContact,
            HasSelectedContact: hasSelectedContact,
            HasApprovedSelectedContact: hasApprovedSelectedContact,
            HasRunnableSelectedContactChat: hasRunnableSelectedContactChat,
            IsConnecting: windowState.IsConnecting,
            HasCurrentProfile: windowState.Onboarding.LocalAccount is not null,
            HasCurrentOrPendingPublicAvatar:
                !string.IsNullOrWhiteSpace(pendingPublicAvatarSourcePath) ||
                pendingPublicAvatarClearRequested ||
                !string.IsNullOrWhiteSpace(windowState.Onboarding.LocalAccount?.PublicAvatarImagePath),
            HasPendingPublicProfileChanges: HasPendingPublicProfileEditorChanges(),
            HasPasswordProtectedRecoveryAccess: currentPasswordProtectionSummary?.Enabled == true,
            HasCurrentProfileId: hasCurrentProfileId,
            IsUpdatingPublicProfile: isUpdatingPublicProfile,
            HasQuickAddContactPetname: !string.IsNullOrWhiteSpace(QuickAddContactPetnameTextBox.Text),
            HasRemotePrekeyBundle: !string.IsNullOrWhiteSpace(RemotePrekeyBundleTextBox.Text),
            HasAddContactIdentifier: !string.IsNullOrWhiteSpace(AddContactIdentifierTextBox.Text),
            HasAddContactPetname: !string.IsNullOrWhiteSpace(AddContactPetnameTextBox.Text),
            HasAddContactPayload: !string.IsNullOrWhiteSpace(AddContactPayloadTextBox.Text),
            IsOnboarding: windowState.Onboarding.ShellMode == AppShellMode.Onboarding,
            IsOnboardingBusy: isOnboardingBusy,
            HasPendingImportFilePath: !string.IsNullOrWhiteSpace(windowState.Onboarding.PendingImportFilePath),
            HasSelectedOnboardingProfile: !string.IsNullOrWhiteSpace(windowState.Onboarding.SelectedProfileId)));

        DiagnosticsPingButton.IsEnabled = controlState.IsPingEnabled;
        ActionStatusTextBlock.Visibility = string.IsNullOrWhiteSpace(ActionStatusTextBlock.Text)
            ? Visibility.Collapsed
            : Visibility.Visible;
        SendButton.IsEnabled = controlState.IsSendEnabled;
        AttachmentMenuButton.IsEnabled = controlState.IsAttachmentMenuEnabled;
        MessageInputTextBox.IsEnabled = controlState.IsMessageInputEnabled;
        DeleteChatContactButton.IsEnabled = controlState.IsDeleteChatContactEnabled;
        DeleteChatContactButton.Visibility = controlState.DeleteChatContactVisibility;
        QuickAddContactButton.IsEnabled = controlState.IsQuickAddContactEnabled;
        ShowLocalPrekeyBundleButton.IsEnabled = controlState.IsShowLocalPrekeyBundleEnabled;
        ImportRemotePrekeyBundleButton.IsEnabled = controlState.IsImportRemotePrekeyBundleEnabled;
        AddContactByIdentifierButton.IsEnabled = controlState.IsAddContactByIdentifierEnabled;
        AddContactFromPayloadButton.IsEnabled = controlState.IsAddContactFromPayloadEnabled;
        RenameContactButton.IsEnabled = controlState.IsRenameContactEnabled;
        RenameContactButton.Visibility = controlState.RenameContactVisibility;
        ContactActionsButton.IsEnabled = controlState.IsContactActionsEnabled;
        ContactActionsButton.Visibility = controlState.ContactActionsVisibility;
        OpenContactChatButton.IsEnabled = controlState.IsOpenContactChatEnabled;
        // Beside the chat itself «Написать» has nowhere to go.
        OpenContactChatButton.Visibility = contactPageInChat ? Visibility.Collapsed : controlState.OpenContactChatVisibility;
        ConfirmVerificationButton.IsEnabled = controlState.IsConfirmVerificationEnabled;
        ConfirmVerificationButton.Visibility = controlState.ConfirmVerificationVisibility;
        SettingsRevealRecoveryButton.IsEnabled = controlState.IsSettingsRevealRecoveryEnabled;
        SettingsSaveRecoveryButton.IsEnabled = controlState.IsSettingsSaveRecoveryEnabled;
        DiagnosticsOpenIsolatedWindowButton.IsEnabled = controlState.IsOpenIsolatedWindowEnabled;
        SettingsSignOutButton.IsEnabled = controlState.IsSettingsSignOutEnabled;
        SettingsDeleteLocalProfileButton.IsEnabled = controlState.IsSettingsDeleteLocalProfileEnabled;
        SettingsPublicDisplayNameTextBox.IsEnabled = controlState.IsSettingsPublicDisplayNameEnabled;
        SettingsChooseAvatarButton.IsEnabled = controlState.IsSettingsChooseAvatarEnabled;
        SettingsClearAvatarButton.IsEnabled = controlState.IsSettingsClearAvatarEnabled;
        // No photo — nothing to take away: the button goes, not greys out.
        SettingsClearAvatarButton.Visibility = controlState.IsSettingsClearAvatarEnabled ? Microsoft.UI.Xaml.Visibility.Visible : Microsoft.UI.Xaml.Visibility.Collapsed;
        SettingsSavePublicProfileButton.IsEnabled = controlState.IsSettingsSavePublicProfileEnabled;
        SettingsSetPasswordButton.IsEnabled = controlState.IsSettingsSetPasswordEnabled;
        SettingsChangePasswordButton.IsEnabled = controlState.IsSettingsChangePasswordEnabled;
        SettingsRemovePasswordButton.IsEnabled = controlState.IsSettingsRemovePasswordEnabled;
        CreateDisplayNameTextBox.IsEnabled = controlState.IsCreateDisplayNameEnabled;
        CreateDeviceLabelTextBox.IsEnabled = controlState.IsCreateDeviceLabelEnabled;
        BootRecoveryResetButton.IsEnabled = controlState.IsBootRecoveryResetEnabled;
        BootRecoveryRetryButton.IsEnabled = controlState.IsBootRecoveryRetryEnabled;
        RestoreDisplayNameTextBox.IsEnabled = controlState.IsRestoreDisplayNameEnabled;
        RestoreRecoveryPhraseTextBox.IsEnabled = controlState.IsRestoreRecoveryPhraseEnabled;
        RestoreDeviceLabelTextBox.IsEnabled = controlState.IsRestoreDeviceLabelEnabled;
        ImportAccountMnemonicTextBox.IsEnabled = controlState.IsImportAccountMnemonicEnabled;
        ImportAccountExecuteButton.IsEnabled = controlState.IsImportAccountExecuteEnabled;
        OpenSavedProfileButton.IsEnabled = controlState.IsOpenSavedProfileEnabled;
        DeleteSavedProfileFromChooserButton.IsEnabled = controlState.IsDeleteSavedProfileFromChooserEnabled;
        UnlockWithPasswordButton.IsEnabled = controlState.IsUnlockWithPasswordEnabled;
        UnlockWithMnemonicButton.IsEnabled = controlState.IsUnlockWithMnemonicEnabled;
        SettingsExportAccountButton.IsEnabled = controlState.IsSettingsExportAccountEnabled;
        bool callActionsEnabled =
            isInitialized &&
            isAuthenticated &&
            lifecycleMessagingAllowed &&
            lifecycleTransportAllowed &&
            nodeModeAllowsNetwork &&
            !windowState.IsConnecting;
        bool canStartCurrentChatCall = callActionsEnabled && !string.IsNullOrWhiteSpace(GetApprovedCurrentDialogKey());
        bool canStartSelectedContactCall = callActionsEnabled && hasApprovedSelectedContact;
        bool canAnswerIncomingCall = callActionsEnabled && GetIncomingBannerCall() is not null;
        CallSummary? actionableCall = GetActionableCurrentChatCall();
        bool hasActiveVideo = callVideoService.IsActiveForDialog(GetResolvedCurrentDialogKey());

        StartCurrentChatCallButton.IsEnabled = canStartCurrentChatCall;
        StartCurrentChatCallButton.Visibility = hasApprovedLinkedChatContact
            ? Visibility.Visible
            : Visibility.Collapsed;
        StartSelectedContactCallButton.IsEnabled = canStartSelectedContactCall;
        StartSelectedContactCallButton.Visibility = hasApprovedSelectedContact
            ? Visibility.Visible
            : Visibility.Collapsed;
        AcceptIncomingCallButton.IsEnabled = canAnswerIncomingCall;
        DeclineIncomingCallButton.IsEnabled = canAnswerIncomingCall;
        AcceptCurrentCallButton.IsEnabled = canAnswerIncomingCall;
        AcceptCurrentCallMutedButton.IsEnabled = canAnswerIncomingCall;
        AcceptIncomingCallMutedButton.IsEnabled = canAnswerIncomingCall;
        DeclineCurrentCallButton.IsEnabled = canAnswerIncomingCall;
        ToggleCurrentCallMuteButton.IsEnabled = callActionsEnabled &&
                                                actionableCall is not null &&
                                                CallOverlayPresenter.IsMicToggleSupported(actionableCall);
        // Camera toggle drives the native media session directly, so it needs that
        // session live for the current dialog (audio-first opt-in) AND a camera that can
        // actually serve the chosen profile — a button that is enabled and then fails
        // when pressed is worse than one that is greyed out with a reason on hover.
        string? cameraDialogKey = GetResolvedCurrentDialogKey();
        bool canStartCamera = callActionsEnabled &&
                              callVideoService.CanStartLocalCameraForDialog(cameraDialogKey);
        ToggleCurrentCallCameraButton.IsEnabled = canStartCamera;
        // Второй жест кнопки (качество по правой кнопке) виден только из подсказки — поэтому она
        // есть и когда кнопка доступна, а не только когда надо объяснить, почему она серая.
        ToolTipService.SetToolTip(
            ToggleCurrentCallCameraButton,
            canStartCamera
                ? "Камера. Правой кнопкой — качество."
                : callVideoService.LocalCameraUnavailableReason);
        // Screen share drives the same native media session as the camera, so gate it identically —
        // only clickable while that session is live for the current dialog. Was ungated (always
        // clickable) — see docs/bugs/live-test-audit-2026-07-18.md.
        ToggleCurrentCallScreenShareButton.IsEnabled = callActionsEnabled &&
                                                       callVideoService.IsActiveForDialog(GetResolvedCurrentDialogKey());
        HangUpCurrentCallButton.IsEnabled = callActionsEnabled &&
                                            (hasActiveVideo ||
                                             (actionableCall is not null &&
                                              CallOverlayPresenter.IsHangUpSupported(actionableCall)));
    }
}
