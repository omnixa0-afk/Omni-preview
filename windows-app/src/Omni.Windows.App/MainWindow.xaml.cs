using System.Collections.ObjectModel;
using System.Text.Json;
using System.Collections.Generic;
using System.IO;
using System.Runtime.InteropServices.WindowsRuntime;
using System.Threading;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml.Media.Imaging;
using Windows.ApplicationModel.DataTransfer;
using Windows.Storage;
using Windows.Storage.FileProperties;
using Windows.Storage.Pickers;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Calls;
using Omni.Windows.App.Features.Chats;
using Omni.Windows.App.Features.Contacts;
using Omni.Windows.App.Features.Profiles;
using Omni.Windows.App.Presentation;
using Omni.Windows.App.ViewModels;

namespace Omni.Windows.App;

public sealed partial class MainWindow : Window
{
    private enum ContactsWorkspaceMode
    {
        Directory,
        Add,
    }

    private enum ContactsFilterMode
    {
        All,
        Pending,
        Approved,
        Declined,
    }

    private static readonly TimeSpan LifecycleRefreshCadence = TimeSpan.FromSeconds(2);
    private static readonly TimeSpan TransportRefreshCadence = TimeSpan.FromSeconds(1);
    private static readonly TimeSpan CallRefreshHotCadence = TimeSpan.FromMilliseconds(500);
    private static readonly TimeSpan CallRefreshIdleCadence = TimeSpan.FromSeconds(2);
    private static readonly TimeSpan ChatHeaderRefreshCadence = TimeSpan.FromMilliseconds(1000);
    private static readonly TimeSpan ChatListRefreshCadence = TimeSpan.FromMilliseconds(1500);
    private static readonly TimeSpan MessageRefreshCadence = TimeSpan.FromMilliseconds(350);
    private static readonly TimeSpan QueueRefreshCadence = TimeSpan.FromMilliseconds(1000);
    private static readonly TimeSpan ContactsRefreshCadence = TimeSpan.FromSeconds(1);
    private static readonly TimeSpan SettingsRefreshCadence = TimeSpan.FromSeconds(2);
    private static readonly TimeSpan FootprintRefreshCadence = TimeSpan.FromSeconds(15);
    private static readonly TimeSpan BackgroundRefreshIdleRecheckCadence = TimeSpan.FromSeconds(5);
    // When the window is unfocused the UI isn't visible, so the refresh loop polls at this coarse
    // cadence instead of the fine per-domain deadlines (cuts idle CPU/battery — threat-model
    // forbids idle overhead). Calls stay live; re-focus forces an immediate full catch-up.
    private static readonly TimeSpan BackgroundUnfocusedCadence = TimeSpan.FromSeconds(3);
    private readonly RustBridge rustBridge;
    private readonly CallVideoService callVideoService;
    private readonly string appDirectory;
    private readonly MainWindowState windowState = new();
    private readonly BackupImportSession backupImportSession = new();
    private readonly ChatRefreshService chatRefreshService = new();
    private readonly StartupPolicyService startupPolicyService = new();
    private readonly ObservableCollection<ChatMessageListItem> messageItems = [];
    private readonly ChatTranscriptPeer transcriptPeer = new();
    private readonly ObservableCollection<ChatThreadListItem> chatThreadItems = [];
    private string chatThreadSearchQuery = string.Empty;
    private readonly Dictionary<string, ChatMessageRecord> currentChatMessagesById = new(StringComparer.Ordinal);
    private readonly Dictionary<string, MessageSecurityDetails> currentChatSecurityDetailsByMessageId = new(StringComparer.Ordinal);
    private readonly Dictionary<string, ChatMessageListItem> currentChatMessageItemsById = new(StringComparer.Ordinal);
    private readonly ObservableCollection<ContactListItem> contactItems = [];
    private readonly ObservableCollection<ContactListItem> filteredContactItems = [];
    private readonly ObservableCollection<ContactListItem> pendingContactItems = [];
    private readonly ObservableCollection<ContactListItem> approvedContactItems = [];
    private readonly ObservableCollection<ContactListItem> declinedContactItems = [];
    private readonly ObservableCollection<SavedProfileListItem> savedProfileItems = [];
    private readonly Dictionary<string, BitmapImage?> avatarImageCache = new(StringComparer.Ordinal);
    private readonly RefreshCoordinator refreshCoordinator;
    private readonly object refreshLoopSync = new();
    private TransportSnapshot? lastTransportSnapshot;
    private QueueDiagnosticsSnapshot? lastQueueDiagnosticsSnapshot;
    private TransportMode cachedConfiguredTransportMode = TransportMode.Quic;
    private ContactCardSnapshot? selectedContactCard;
    private ChatHeaderContactSummary? currentChatHeaderSummary;
    private ChatSecuritySummary? currentChatSecuritySummary;
    private ChatSessionSummary? currentChatSessionSummary;
    // How long the window stays open after the X while media shuts down. See OnAppWindowClosing.
    private const int ShutdownGraceMs = 5_000;
    // How long the window gets to actually disappear after teardown before the process ends itself.
    private const int ShutdownExitGraceMs = 1_500;
    private bool shutdownStarted;
    private bool shutdownComplete;
    private CallSummary? currentChatCallSummary;
    private CallInboxSnapshot? currentCallInboxSnapshot;
    // Dialog whose call media is being started right now, so the 500 ms call refresh doesn't
    // start a second one on top of it. UI thread only. See StartCallMediaWhenCallIsAnswered.
    private string? callMediaStartingForDialogKey;
    private DangerousActionPreview? pendingDangerousActionPreview;
    private ProfileGateSummary? currentProfileGateSummary;
    private ProfileLifecycleSummary? currentProfileLifecycleSummary;
    private LocalProfileFootprintSummary? currentProfileFootprintSummary;
    private PasswordProtectionSummary? currentPasswordProtectionSummary;
    private string? currentPasswordProtectionErrorText;
    private readonly MenuFlyout chatHeaderMenuFlyout = new();
    private readonly MenuFlyoutItem endChatSessionMenuItem = new();
    private readonly MenuFlyout contactActionsFlyout = new();
    private readonly MenuFlyout attachmentActionsFlyout = new();
    private readonly IReadOnlyList<CallVideoCodecPreferenceOption> callVideoCodecPreferenceOptions =
    [
        new CallVideoCodecPreferenceOption(CallVideoCodecPreference.Auto, "Авто — лучший для каждого собеседника"),
        new CallVideoCodecPreferenceOption(CallVideoCodecPreference.UpToH265, "Не выше HEVC"),
        new CallVideoCodecPreferenceOption(CallVideoCodecPreference.H264, "Только H.264"),
    ];
    private readonly IReadOnlyList<CallVideoBitratePreferenceOption> callVideoBitratePreferenceOptions =
    [
        new CallVideoBitratePreferenceOption(CallVideoBitratePreference.Economy, "Экономно — вдвое меньше"),
        new CallVideoBitratePreferenceOption(CallVideoBitratePreference.Auto, "Авто"),
        new CallVideoBitratePreferenceOption(CallVideoBitratePreference.Max, "Максимум — в полтора раза больше"),
    ];
    private readonly IReadOnlyList<CallVideoFrameRatePreferenceOption> callVideoFrameRatePreferenceOptions =
    [
        new CallVideoFrameRatePreferenceOption(CallVideoFrameRatePreference.Auto, "Авто"),
        new CallVideoFrameRatePreferenceOption(CallVideoFrameRatePreference.Fps30, "30 fps"),
        new CallVideoFrameRatePreferenceOption(CallVideoFrameRatePreference.Fps60, "60 fps"),
    ];
    private string lastRenderedPeerSignature = string.Empty;
    private string lastRenderedDiscoveryNetworkSettingsSignature = string.Empty;
    private bool suppressChatSelectionSync;
    private bool suppressContactSelectionSync;
    private bool suppressCallVideoPreferenceSelection;
    private ContactsWorkspaceMode currentContactsWorkspaceMode = ContactsWorkspaceMode.Directory;
    private ContactsFilterMode currentContactsFilterMode = ContactsFilterMode.All;
    private bool isOnboardingBusy;
    private bool bootStarted;

    // Window focus state — drives coarse background polling when the user isn't looking.
    private bool isWindowActive = true;
    private string lastRenderedChatHeaderSignature = string.Empty;
    private string? pendingAttachmentSourcePath;
    private string? pendingAttachmentFileName;
    private long pendingAttachmentFileSize;
    // The draft goes as a photo (re-encoded, shown in the chat) rather than as the file itself.
    private bool pendingAttachmentAsPhoto;
    private string? pendingPublicAvatarSourcePath;
    private string? pendingPublicAvatarDisplayName;
    private bool pendingPublicAvatarClearRequested;
    private bool isUpdatingPublicProfile;
    private bool pendingChatAutoScrollToLatest;
    private DateTimeOffset nextLifecycleRefreshAt = DateTimeOffset.MinValue;
    private DateTimeOffset nextTransportRefreshAt = DateTimeOffset.MinValue;
    private DateTimeOffset nextCallRefreshAt = DateTimeOffset.MinValue;
    private DateTimeOffset nextChatHeaderRefreshAt = DateTimeOffset.MinValue;
    private DateTimeOffset nextChatListRefreshAt = DateTimeOffset.MinValue;
    private DateTimeOffset nextMessageRefreshAt = DateTimeOffset.MinValue;
    private DateTimeOffset nextQueueRefreshAt = DateTimeOffset.MinValue;
    private DateTimeOffset nextContactsRefreshAt = DateTimeOffset.MinValue;
    private DateTimeOffset nextSettingsRefreshAt = DateTimeOffset.MinValue;
    private DateTimeOffset nextFootprintRefreshAt = DateTimeOffset.MinValue;
    private int refreshLoopGeneration;
    private int refreshDispatchQueued;
    private BackgroundRefreshDomain pendingRefreshWakeDomains = BackgroundRefreshDomain.None;
    private CancellationTokenSource? refreshLoopCancellation;
    private readonly SemaphoreSlim refreshLoopWakeSignal = new(0, 1);

    public MainWindow(string? appDirectory = null)
    {
        this.appDirectory = string.IsNullOrWhiteSpace(appDirectory)
            ? RustBridge.GetDefaultAppDirectory()
        : Path.GetFullPath(appDirectory);
        rustBridge = new RustBridge(this.appDirectory);
        MediaFiles.Attach(rustBridge);
        CleanupUiTempDirectory();
        InitializeComponent();
        callVideoService = new CallVideoService(
            rustBridge,
            DispatcherQueue,
            LocalCallVideoImage,
            RemoteCallVideoImage,
            RemoteCallVideoSwapChainPanel,
            CallVideoDiagnosticsTextBlock);
        callVideoService.StateChanged += OnCallVideoServiceStateChanged;
#if OMNI_DEV_TOOLS
        // Dev-only, no-op unless OMNI_UI_TEST_CHANNEL points at a directory. Not compiled into Release.
        StartUiTestChannelIfEnabled();
#endif
        WireSectionEvents();
        refreshCoordinator = new RefreshCoordinator(
            DispatcherQueue,
            FetchBackgroundRefreshAsync,
            ApplyBackgroundRefresh);
        Title = "Omni";
        ApplyWindowIcon();
        MenuFlyoutItem openContactProfileMenuItem = new()
        {
            Text = "Профиль",
            Icon = new FontIcon { Glyph = "\uE77B" },
        };
        openContactProfileMenuItem.Click += (_, _) => OpenCurrentChatContactProfile();
        chatHeaderMenuFlyout.Items.Add(openContactProfileMenuItem);
        endChatSessionMenuItem.Text = "Завершить сессию";
        AutomationProperties.SetAutomationId(endChatSessionMenuItem, "EndChatSessionMenuItem");
        endChatSessionMenuItem.IsEnabled = false;
        endChatSessionMenuItem.Click += OnEndCurrentChatSessionClicked;
        chatHeaderMenuFlyout.Items.Add(endChatSessionMenuItem);
        chatHeaderMenuFlyout.Items.Add(new MenuFlyoutSeparator());
        MenuFlyoutItem deleteChatContactMenuItem = new()
        {
            Text = "Забыть контакт",
        };
        deleteChatContactMenuItem.Click += OnDeleteCurrentChatContactClicked;
        chatHeaderMenuFlyout.Items.Add(deleteChatContactMenuItem);

        MessagesListView.ItemsSource = messageItems;
        ChatsListView.ItemsSource = chatThreadItems;
        FilteredContactsListView.ItemsSource = filteredContactItems;
        SavedProfilesListView.ItemsSource = savedProfileItems;

        LoadUiState();

        Activated += OnActivated;
        // Teardown runs from Closing, NOT from Closed. Closed fires after the window is gone and
        // its dispatcher has stopped pumping, so anything in there that needs the UI thread —
        // and media teardown does, it has to unbind the swapchain and drop the preview bitmaps —
        // waits for a queue nobody drains any more. Pressing the X during a call left the media
        // stop half-finished and the process alive with the Rust runtime still running.
        AppWindow.Closing += OnAppWindowClosing;
        Closed += OnClosed;

        SetShellMode(AppShellMode.Booting);
        QueueSummaryTextBlock.Text = "Очередь сообщений недоступна.";
        QueueDiagnosticsTextBlock.Text = "Очередь сообщений недоступна.";
        SetActionStatus(string.Empty);
        ContactsStatusTextBlock.Text = string.Empty;
        ContactManagementStatusTextBlock.Text = string.Empty;
        ChatIdentityTitleTextBlock.Text = "Диалог";
        ChatIdentitySubtitleTextBlock.Text = "Выберите контакт слева или отправьте запрос.";
        ChatConnectionStatusTextBlock.Text = "Статус: диалог не выбран";
        ChatTrustWarningTextBlock.Text = string.Empty;
        ChatTrustWarningTextBlock.Visibility = Visibility.Collapsed;
        LocalPrekeyBundleTextBox.Text = "Сведения о ключах скрыты из обычного чата.";
        RemotePrekeyBundleTextBox.Text = string.Empty;
        PrekeyBundleStatusTextBlock.Text = "Статус: диалог не выбран";
        ChatSecuritySummaryTextBlock.Text = "Сведения о шифровании появятся после выбора или запуска диалога.";
        SecurityDiagnosticsTextBlock.Text = "Сводка безопасности для текущего диалога недоступна.";
        SettingsProfileSummaryTextBlock.Text = "Локальный профиль ещё не выбран.";
        SettingsProfileEditorStatusTextBlock.Text = "Публичное имя и аватар можно обновить после входа в профиль.";
        SettingsPasswordSummaryTextBlock.Text = "Пароль не задан.";
        DiagnosticsPingStatusTextBlock.Text = "Проверка ядра пока не запускалась.";
        SettingsExportStatusTextBlock.Text = "Резервная копия аккаунта пока не сохранена.";
        ImportAccountFileNameTextBlock.Text = "Файл не выбран.";
        ImportAccountFileSummaryTextBlock.Text = string.Empty;
        ImportAccountStatusTextBlock.Text = "Выберите файл копии и введите фразу восстановления.";
        InitializeConnectionPrivacyPicker();
        InitializeCarrierPolicyPicker();
        InitializeDirectTcpPortField();
        InitializeNetworkNodes();
        InitializePublicRelays();
        InitializePublishPlans();
        InitializeBackupReminder();
        InitializeRendezvousProviders();
        RefreshDiscoverabilitySettings();
        InitializeCallVideoPreferencePickers();
        InitializeCameraPicker();
        InitializeAudioDevicePickers();
        InitializeThemePicker();
        InitializeBackgroundMode();
        InitializeCongestionPicker();
        ApplyPersistedCallVideoDevicePreferences();
        ApplyContactsWorkspaceMode(currentContactsWorkspaceMode);
        UpdateContactsFilterButtons();
        RefreshFilteredContactsList(allowFilterShiftForSelection: false);
        UpdateDraftAttachmentUi();
        UpdateCurrentPeerText();
        UpdateControlStates();
    }

    private async void OnActivated(object sender, WindowActivatedEventArgs args)
    {
        bool active = args.WindowActivationState != WindowActivationState.Deactivated;
        bool becameActive = active && !isWindowActive;
        isWindowActive = active;

        // Regained focus: we polled coarsely while unfocused, so catch the UI up immediately.
        if (becameActive && windowState.Onboarding.ShellMode == AppShellMode.Authenticated)
        {
            WakeAuthenticatedRefreshLoop(BackgroundRefreshDomain.All);
            // Back to the window: the chat open in it has now been seen.
            ReadChatOnScreen();
        }

        if (bootStarted)
        {
            return;
        }

        bootStarted = true;
        await BootAsync();
    }

    /// <summary>
    /// Started without a window (OMNI_START_HIDDEN, a test stand): the core boots now, as it would on
    /// the first activation — which never comes until someone opens the window.
    /// </summary>
    internal void BootWithoutWindow()
    {
        if (bootStarted)
        {
            return;
        }

        bootStarted = true;
        _ = BootAsync();
    }

    private async Task BootAsync()
    {
        StopAuthenticatedRefreshLoop();
        SetShellMode(AppShellMode.Booting);
        BootStatusTextBlock.Text = "Инициализируем локальное ядро и проверяем профиль...";

        try
        {
            bool legacyDataPurged = false;
            while (true)
            {
                // A data folder from another machine: its key is sealed there, the phrase opens it here.
                if (await Task.Run(() => rustBridge.DatabaseNeedsRecoveryPhrase()))
                {
                    if (!await OpenDatabaseFromAnotherMachineAsync())
                    {
                        return;
                    }

                    continue;
                }

                var bootstrapSummary = await Task.Run(() => rustBridge.InspectDatabaseBootstrap());
                if (!bootstrapSummary.Supported)
                {
                    if (legacyDataPurged)
                    {
                        savedProfileItems.Clear();
                        windowState.Onboarding.LocalAccount = null;
                        windowState.Onboarding.LocalDevice = null;
                        windowState.Onboarding.ActiveProfileId = null;
                        windowState.Onboarding.SelectedProfileId = null;
                        windowState.Onboarding.PendingRecoveryPhraseV2 = null;
                        currentProfileLifecycleSummary = null;
                        currentProfileGateSummary = null;
                        currentProfileFootprintSummary = null;
                        currentPasswordProtectionSummary = null;
                        currentPasswordProtectionErrorText = null;
                        UpdateIdentityHeader();
                        ShowOnboardingStep(
                            OnboardingStep.Welcome,
                            string.IsNullOrWhiteSpace(bootstrapSummary.UnsupportedReason)
                                ? "Старая локальная база данных не поддерживается."
                                : bootstrapSummary.UnsupportedReason);
                        return;
                    }

                    BootStatusTextBlock.Text = "Найдены устаревшие локальные данные. Удаляем их и запускаем приложение заново...";
                    await Task.Run(() => rustBridge.ResetLocalData());
                    legacyDataPurged = true;
                    continue;
                }

                await Task.Run(() => rustBridge.Init());

                SavedProfileList savedProfiles = rustBridge.ListLocalProfiles();
                PopulateSavedProfiles(savedProfiles);
                windowState.Onboarding.LocalAccount = rustBridge.GetLocalAccount();
                windowState.Onboarding.LocalDevice = rustBridge.GetLocalDevice();
                RefreshCurrentLifecycleContracts();
                windowState.Onboarding.ActiveProfileId = rustBridge.GetActiveProfileId();
                UpdateIdentityHeader();
                bool hasSavedProfiles = savedProfiles.Profiles.Count > 0;
                bool hasSignedInProfile = currentProfileLifecycleSummary?.IsUnlocked == true;

                if (!hasSavedProfiles)
                {
                    ShowOnboardingStep(
                        OnboardingStep.Welcome,
                        string.Empty);
                    return;
                }

                if (hasSavedProfiles && !hasSignedInProfile)
                {
                    ShowOnboardingStep(
                        OnboardingStep.ProfileChooser,
                        "Выберите сохранённый профиль, чтобы сделать его активным.");
                    return;
                }

                EnterAuthenticatedShell(string.Empty);
                return;
            }
        }
        catch (Exception ex)
        {
            string userMessage = NormalizeUserError(ex);
            if (ShouldOfferDatabaseResetForInitFailure(userMessage))
            {
                ShowOnboardingStep(
                    OnboardingStep.BootRecovery,
                    $"Не удалось подготовить приложение: {userMessage}");
                return;
            }

            ShowOnboardingStep(
                OnboardingStep.Welcome,
                $"Не удалось подготовить приложение: {userMessage}");
        }
    }























































    private static void SynchronizeCollection<TItem>(
        ObservableCollection<TItem> target,
        IReadOnlyList<TItem> source,
        Func<TItem, string> keySelector)
    {
        Dictionary<string, int> sourceIndexes = new(StringComparer.Ordinal);
        foreach (TItem item in source)
        {
            string key = keySelector(item);
            if (!sourceIndexes.ContainsKey(key))
            {
                sourceIndexes[key] = sourceIndexes.Count;
            }
        }

        for (int index = target.Count - 1; index >= 0; index--)
        {
            string key = keySelector(target[index]);
            if (!sourceIndexes.ContainsKey(key))
            {
                target.RemoveAt(index);
            }
        }

        for (int index = 0; index < source.Count; index++)
        {
            TItem sourceItem = source[index];
            string sourceKey = keySelector(sourceItem);

            if (index >= target.Count)
            {
                target.Insert(index, sourceItem);
                continue;
            }

            string currentKey = keySelector(target[index]);
            if (!string.Equals(currentKey, sourceKey, StringComparison.Ordinal))
            {
                int existingIndex = -1;
                for (int searchIndex = index + 1; searchIndex < target.Count; searchIndex++)
                {
                    if (string.Equals(keySelector(target[searchIndex]), sourceKey, StringComparison.Ordinal))
                    {
                        existingIndex = searchIndex;
                        break;
                    }
                }

                if (existingIndex >= 0)
                {
                    target.Move(existingIndex, index);
                }
                else
                {
                    target.Insert(index, sourceItem);
                }
            }

            if (!EqualityComparer<TItem>.Default.Equals(target[index], sourceItem))
            {
                target[index] = sourceItem;
            }
        }

        while (target.Count > source.Count)
        {
            target.RemoveAt(target.Count - 1);
        }
    }




















    /// <summary>
    /// The X was pressed. Hold the window open just long enough to shut the media down while the
    /// dispatcher is still alive, then close for real.
    ///
    /// The wait is bounded: a native handle that refuses to let go (the audio capture stops and
    /// JOINS its worker thread) must not turn "close the app" into a window that never goes away.
    /// Past the deadline the process exits anyway — the OS reclaims what we failed to release.
    /// </summary>
    private void OnAppWindowClosing(AppWindow sender, AppWindowClosingEventArgs args)
    {
        if (shutdownComplete)
        {
            return;
        }

        if (!shutdownStarted && TryHideToBackground(args))
        {
            return;
        }

        args.Cancel = true;
        BeginShutdown();
    }

    private async Task ShutDownThenCloseAsync()
    {
        try
        {
            StopAuthenticatedRefreshLoop();
            TryStopMeshOverlay();
            CancelTranscriptLoads();
            SaveUiState();
            DisposeTrayIcon();
            DisposeRemoteControlBar();
            refreshCoordinator.Dispose();

            Task mediaStop = callVideoService.DisposeAsync().AsTask();
            if (await Task.WhenAny(mediaStop, Task.Delay(ShutdownGraceMs)) != mediaStop)
            {
                AppTrace.Write("app/shutdown", $"media stop did not finish in {ShutdownGraceMs}ms; closing anyway");
            }

            rustBridge.Dispose();
        }
        catch (Exception ex)
        {
            AppTrace.WriteFailure("app/shutdown", "teardown", ex);
        }

        AppTrace.Write("app/shutdown", "closing: done");
        shutdownComplete = true;
        try
        {
            Close();
        }
        catch (Exception ex)
        {
            // Worth recording rather than swallowing: the first version of this let the exception
            // escape into an unobserved task, and the window simply stayed open with the app
            // still responding — teardown finished, nothing closed, and nothing said why.
            AppTrace.WriteFailure("app/shutdown", "Close()", ex);
        }

        // Backstop, deliberately independent of the dispatcher and of Close() succeeding. The live
        // gate caught the window surviving a close during a call (C8, 2026-08-12): teardown
        // finished in 291 ms and the window stayed. By this point the app has released everything
        // it owns — media stopped, native sessions disposed, the Rust runtime shut down — so if
        // the window is still up after the grace period, the process ends. The user asked to
        // close the app; "the window would not go" is not a reason to keep running.
        _ = Task.Run(async () =>
        {
            await Task.Delay(ShutdownExitGraceMs);
            AppTrace.Write("app/shutdown", "window survived Close(); exiting the process");
            Environment.Exit(0);
        });
    }

    private void OnClosed(object sender, WindowEventArgs args)
    {
        // Everything real happens in OnAppWindowClosing. This is the path for a close that never
        // went through it (a shutdown driven by the OS, say) — best effort, nothing awaited,
        // because by now the dispatcher no longer runs what we would await.
        if (shutdownComplete)
        {
            return;
        }

        AppTrace.Write("app/shutdown", "closed without a closing pass; best-effort teardown");
        DisposeTrayIcon();
        DisposeRemoteControlBar();
        StopAuthenticatedRefreshLoop();
        TryStopMeshOverlay();
        rustBridge.Dispose();
    }
}
