using System.IO;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Omni.Windows.App.ViewModels;

namespace Omni.Windows.App;

public sealed partial class MainWindow
{
    private void ShowOnboardingStep(OnboardingStep step, string statusText)
    {
        windowState.Onboarding.ShellMode = AppShellMode.Onboarding;
        windowState.Onboarding.Step = step;

        SetShellMode(AppShellMode.Onboarding);
        OnboardingStatusTextBlock.Text = statusText;

        WelcomePanel.Visibility = step == OnboardingStep.Welcome ? Visibility.Visible : Visibility.Collapsed;
        BootRecoveryPanel.Visibility = step == OnboardingStep.BootRecovery ? Visibility.Visible : Visibility.Collapsed;
        ProfileChooserPanel.Visibility = step == OnboardingStep.ProfileChooser ? Visibility.Visible : Visibility.Collapsed;
        UnlockSavedProfilePanel.Visibility = step == OnboardingStep.UnlockSavedProfile ? Visibility.Visible : Visibility.Collapsed;
        CreateAccountPanel.Visibility = step == OnboardingStep.CreateAccount ? Visibility.Visible : Visibility.Collapsed;
        RecoveryPanel.Visibility = step == OnboardingStep.RecoveryPhrase ? Visibility.Visible : Visibility.Collapsed;
        ImportAccountPanel.Visibility = step == OnboardingStep.ImportAccount ? Visibility.Visible : Visibility.Collapsed;
        RestoreAccountPanel.Visibility = step == OnboardingStep.RestoreAccount ? Visibility.Visible : Visibility.Collapsed;

        (string title, string subtitle) = step switch
        {
            // Said as a person would (owner, 2026-10-13: UI/UX pass) — the restore line used to read
            // «…только если Rust разрешит restore».
            OnboardingStep.BootRecovery => ("Omni не запустился", "Данные этого окна не открываются. Можно стереть их и начать заново."),
            OnboardingStep.ProfileChooser => ("Кто вы?", "Выберите профиль, чтобы войти."),
            OnboardingStep.UnlockSavedProfile => ("Вход", "Введите пароль — или фразу восстановления, если пароль забыт."),
            OnboardingStep.CreateAccount => ("Новый профиль", "Как вас зовут и как назвать этот компьютер."),
            OnboardingStep.RecoveryPhrase => ("Сохраните фразу", "24 слова — единственный способ вернуть аккаунт, если компьютер пропадёт."),
            OnboardingStep.ImportAccount => ("Восстановление из копии", "Файл резервной копии и фраза восстановления вернут переписку и контакты."),
            OnboardingStep.RestoreAccount => ("Восстановление по фразе", "Введите 24 слова — аккаунт вернётся на этот компьютер, а контакты и переписка подтянутся с ваших устройств и от собеседников."),
            _ => ("Добро пожаловать", "Создайте профиль — или верните свой на этот компьютер."),
        };

        OnboardingTitleTextBlock.Text = title;
        OnboardingSubtitleTextBlock.Text = subtitle;
        UpdateControlStates();
    }

    private void LoadUiState()
    {
        try
        {
            DeleteLegacyUiStateFile();
            windowState.CurrentPeerKey = null;
            windowState.CurrentTransportPeerKey = null;
        }
        catch
        {
            windowState.CurrentPeerKey = null;
            windowState.CurrentTransportPeerKey = null;
        }
    }

    private void SaveUiState()
    {
        // Legacy UI state persistence was removed.
    }

    private string GetUiStatePath()
        => Path.Combine(rustBridge.AppDirectory, "ui-state.json");

    private void DeleteLegacyUiStateFile()
    {
        string path = GetUiStatePath();
        if (!File.Exists(path))
        {
            return;
        }

        File.Delete(path);
    }

    private const uint WM_SETICON = 0x0080;
    private const nint ICON_SMALL = 0;
    private const nint ICON_BIG = 1;

    [System.Runtime.InteropServices.DllImport("shell32.dll", CharSet = System.Runtime.InteropServices.CharSet.Unicode)]
    private static extern uint ExtractIconEx(string file, int index, out nint large, out nint small, uint count);

    [System.Runtime.InteropServices.DllImport("user32.dll")]
    private static extern nint SendMessage(nint hwnd, uint message, nint wParam, nint lParam);

    /// <summary>
    /// The window's own icon — the title bar takes the small one, the taskbar button and Alt+Tab the
    /// big one. They come from the WINDOW, not from the exe: with Omni.ico in the exe both still
    /// showed Windows' blank app icon (live 2026-10-17). Read from the exe's own icon resource
    /// because the single-file build has no .ico on disk. The handles live as long as the window.
    /// </summary>
    private void ApplyWindowIcon()
    {
        string? exe = Environment.ProcessPath;
        if (exe is null || ExtractIconEx(exe, 0, out nint large, out nint small, 1) == 0)
        {
            return;
        }

        nint hwnd = WinRT.Interop.WindowNative.GetWindowHandle(this);
        if (small != 0)
        {
            _ = SendMessage(hwnd, WM_SETICON, ICON_SMALL, small);
        }

        if (large != 0)
        {
            _ = SendMessage(hwnd, WM_SETICON, ICON_BIG, large);
        }
    }

    private void OnAuthenticatedNavigationSelectionChanged(NavigationView sender, NavigationViewSelectionChangedEventArgs args)
    {
        string? tag = (args.SelectedItem as NavigationViewItem)?.Tag as string;
        windowState.CurrentSection = tag switch
        {
            "Contacts" => AuthenticatedShellSection.Contacts,
            "Settings" => AuthenticatedShellSection.Settings,
            "Diagnostics" => AuthenticatedShellSection.Diagnostics,
            _ => AuthenticatedShellSection.Chats,
        };

        // «Контакты» get their page back.
        if (windowState.CurrentSection != AuthenticatedShellSection.Chats)
        {
            CloseChatContactInfo();
        }

        ChatsSectionRoot.Visibility = windowState.CurrentSection == AuthenticatedShellSection.Chats ? Visibility.Visible : Visibility.Collapsed;
        ContactsSectionRoot.Visibility = windowState.CurrentSection == AuthenticatedShellSection.Contacts ? Visibility.Visible : Visibility.Collapsed;
        SettingsSectionRoot.Visibility = windowState.CurrentSection == AuthenticatedShellSection.Settings ? Visibility.Visible : Visibility.Collapsed;
        DiagnosticsSectionRoot.Visibility = windowState.CurrentSection == AuthenticatedShellSection.Diagnostics ? Visibility.Visible : Visibility.Collapsed;
        // The badge on the gear is a new device of the account: that is where the click goes.
        if (windowState.CurrentSection is AuthenticatedShellSection.Settings && HasNewDevicesBadge)
        {
            SettingsSectionHost.ShowSection("devices");
        }

        // Per-section refresh runs synchronous FFI on the UI thread. A transient core error
        // (e.g. SQLite busy under background write contention) MUST NOT escape into this
        // NavigationView event handler — WinUI 3 turns an unhandled event-handler exception
        // into a silent process kill. Each refresh step is guarded; the exception is logged
        // and the section still transitions visually.
        if (windowState.CurrentSection is AuthenticatedShellSection.Contacts)
        {
            TryRefresh("contacts.transport_status", RefreshTransportStatus);
            // Heavy full-list read OFF the UI thread so switching TO Contacts flips instantly instead of
            // freezing on a blocking FFI read under background write contention (list fills a moment later).
            LoadContactsListAsync(windowState.SelectedContactId);
            TryRefresh("contacts.selected_card", RefreshSelectedContactCard);
            TryRefresh("contacts.identity", UpdateIdentityHeader);
            TryRefresh("contacts.controls", UpdateControlStates);
        }
        else if (windowState.CurrentSection is AuthenticatedShellSection.Chats)
        {
            // Heavy full-list read OFF the UI thread so switching TO Chats flips instantly (see Contacts).
            LoadChatsListAsync(windowState.CurrentPeerKey);
            TryRefresh("chats.header", RefreshChatHeaderSummary);
            TryRefresh("chats.messages", () => RefreshMessages(force: true));
            TryRefresh("chats.queue", RefreshQueueDiagnostics);
            TryRefresh("chats.identity", UpdateIdentityHeader);
            TryRefresh("chats.controls", UpdateControlStates);
        }
        else if (windowState.CurrentSection is AuthenticatedShellSection.Settings or AuthenticatedShellSection.Diagnostics)
        {
            if (windowState.CurrentSection is AuthenticatedShellSection.Settings)
            {
                // First Settings view: populate the HW encode/decode device combos and the camera
                // list lazily so the capability probe (MF/QSV/NVENC) and camera enumeration (MF
                // sensor stack) stay off the boot path until the user actually opens Settings.
                TryRefresh("settings.device_pickers", EnsureCallVideoDevicePickersPopulated);
                TryRefresh("settings.camera_picker", EnsureCameraPickerPopulated);
                TryRefresh("settings.audio_devices", RefreshAudioDevicePickers);
            }

            TryRefresh("settings.transport_status", RefreshTransportStatus);
            TryRefresh("settings.profile_footprint", RefreshCurrentProfileFootprintIfVisible);
            TryRefresh("settings.identity", UpdateIdentityHeader);
            TryRefresh("settings.controls", UpdateControlStates);
        }

        TryRefresh("shell.wake_section", () => WakeShellSectionRefresh(windowState.CurrentSection));
        TryRefresh("shell.save_ui_state", SaveUiState);
    }

    /// Run a refresh step and, on failure, log the exception + optionally surface a status message,
    /// but NEVER re-throw into the event handler. The nav handler is a WinUI 3 event callback —
    /// escaping there terminates the process silently (live-observed 2026-07-20 on Contacts after
    /// forget). One-liner for the nav block so a new refresh step cannot forget the guard.
    private void TryRefresh(string operation, System.Action work, string? statusMessage = null)
    {
        try
        {
            work();
        }
        catch (Exception ex)
        {
            AppendNavRefreshLog(operation, ex);
            if (statusMessage is not null)
            {
                try { ContactManagementStatusTextBlock.Text = statusMessage; }
                catch { /* the status block might not exist on this section — best effort */ }
            }
        }
    }

    private void AppendNavRefreshLog(string operation, Exception ex)
    {
        try
        {
            string logsDirectory = Path.Combine(appDirectory, "logs");
            Directory.CreateDirectory(logsDirectory);
            string logPath = Path.Combine(logsDirectory, "nav-refresh.log");
            string line = $"{DateTimeOffset.Now:O} operation={operation} exception_type={ex.GetType().FullName} message={ex.Message}";
            File.AppendAllText(logPath, line + Environment.NewLine);
        }
        catch
        {
            // The log-writer itself must never throw — that would defeat the whole guard.
        }
    }

    private async void OnOpenIsolatedWindowClicked(object sender, RoutedEventArgs e)
    {
        try
        {
            TransportMode currentMode = GetCurrentTransportMode();
            SetLaunchWindowStatus("Создаём изолированное окно для второго пира...");
            string workspaceDirectory = await Task.Run(
                () => rustBridge.CreateSameHostCompanionWorkspaceDirectory(Guid.NewGuid().ToString("N")));
            string? processPath = Environment.ProcessPath;
            if (string.IsNullOrWhiteSpace(processPath))
            {
                throw new InvalidOperationException("Не удалось определить путь к приложению.");
            }

            System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo
            {
                FileName = processPath,
                Arguments = $"--app-dir \"{workspaceDirectory}\"",
                WorkingDirectory = Path.GetDirectoryName(processPath) ?? AppContext.BaseDirectory,
                UseShellExecute = false,
            });

            RefreshTransportStatus();
            UpdateControlStates();

            SetLaunchWindowStatus(
                $"Открыто новое окно {Path.GetFileName(workspaceDirectory)} в режиме {currentMode.ToDisplayName()}.");
        }
        catch (Exception ex)
        {
            SetLaunchWindowStatus($"Не удалось открыть новое окно: {NormalizeUserError(ex)}");
        }
    }

    private TransportMode GetCurrentTransportMode() => cachedConfiguredTransportMode;
}
