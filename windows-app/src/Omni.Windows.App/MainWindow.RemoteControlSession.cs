using System;
using System.Threading.Tasks;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Calls;
using Omni.Windows.App.Presentation;

namespace Omni.Windows.App;

// Remote-control sessions over standing remote access (docs/decisions/remote-access-for-contacts.md).
// A session is a call of its own kind — no microphone, no camera — and reuses the in-call screen
// control (docs/decisions/remote-screen-control.md) for the picture and the signed input.
//
// Controller: "Управлять компьютером" in the chat header and the contact card → the call panel shows
// the owner's screen; control starts by itself once the owner grants it, "Только смотреть" pauses it,
// "Во весь экран" takes the whole display.
// Owner: the core answers (unattended) or the banner asks; once approved, the primary monitor is
// shared without the system picker (consent was given with the grant) and input is allowed to this
// call; an always-on-top bar says who controls the computer, with "Остановить", for the whole session.
public sealed partial class MainWindow
{
    /// The controller chose "Только смотреть" in this session; control does not restart by itself.
    private bool remoteControlViewOnly;
    private string? remoteControlSessionCallId;
    private string? ownerShareStartedForCallId;
    private string? surfacedIncomingCallId;
    private RemoteControlBar? remoteControlBar;
    private bool callFullScreen;
    /// Owner: the session holds the display on; released when it ends.
    private bool ownerKeepsDisplayOn;
    private string? lastReportedSharerState;

    // --- controller: where a session starts -------------------------------------------------------

    private void OnStartCurrentChatRemoteControlClicked(object sender, RoutedEventArgs e)
    {
        ContactSnapshot? contact = currentChatHeaderSummary?.ContactCard?.Contact;
        StartRemoteControlWith(contact);
    }

    private void OnStartSelectedContactRemoteControlClicked(object sender, RoutedEventArgs e) =>
        StartRemoteControlWith(selectedContactCard?.Contact);

    private void StartRemoteControlWith(ContactSnapshot? contact)
    {
        if (contact is null)
        {
            return;
        }

        string name = DisplayNameOf(contact);
        try
        {
            CallSummary call = rustBridge.RequestRemoteControl(contact.ContactId);
            remoteControlViewOnly = false;
            ShowChatsSectionForDialog(call.DialogKey, contact.ContactId);
            RefreshCallInbox();
            SetActionStatus($"Подключаемся к компьютеру «{name}»…");
        }
        catch (Exception ex)
        {
            SetActionStatus(DescribeRemoteControlFailure(NormalizeUserError(ex), name));
        }
    }

    /// What went wrong, in the user's terms — the core's reasons are for logs.
    internal static string DescribeRemoteControlFailure(string error, string name)
    {
        string text = error ?? string.Empty;
        if (text.Contains("no live path", StringComparison.OrdinalIgnoreCase) ||
            text.Contains("stopped answering", StringComparison.OrdinalIgnoreCase) ||
            text.Contains("no established session", StringComparison.OrdinalIgnoreCase))
        {
            return $"Компьютер «{name}» сейчас не в сети. Попробуйте, когда он появится.";
        }

        if (text.Contains("already running", StringComparison.OrdinalIgnoreCase))
        {
            return $"С «{name}» уже идёт звонок: попросите показать экран и разрешить управление прямо в звонке.";
        }

        if (text.Contains("has not allowed", StringComparison.OrdinalIgnoreCase))
        {
            return $"«{name}» не разрешал управлять своим компьютером.";
        }

        if (text.Contains("another call", StringComparison.OrdinalIgnoreCase))
        {
            return "Сначала завершите текущий звонок.";
        }

        return $"Не удалось подключиться к компьютеру «{name}»: {text}";
    }

    /// The header / card button: shown only when this contact lets us control its computer, greyed
    /// with the reason while it is offline.
    private void RenderRemoteControlAffordance(Button button, ContactSnapshot? contact)
    {
        string? received = null;
        if (contact is not null)
        {
            try
            {
                received = rustBridge.GetContactRemoteAccess(contact.ContactId).ReceivedMode;
            }
            catch (Exception)
            {
                received = null; // profile not ready: hide; the next render retries
            }
        }

        if (contact is null || received is null)
        {
            button.Visibility = Visibility.Collapsed;
            return;
        }

        bool online = contact.RemoteAccountId is { } account && CurrentlyLinkedAccountIds().Contains(account);
        string name = DisplayNameOf(contact);
        button.Visibility = Visibility.Visible;
        button.IsEnabled = online;
        ToolTipService.SetToolTip(button, online
            ? $"Управлять компьютером «{name}»"
            : $"Компьютер «{name}» не в сети");
    }

    private void RenderChatRemoteControlAffordance() =>
        RenderRemoteControlAffordance(StartCurrentChatRemoteControlButton, currentChatHeaderSummary?.ContactCard?.Contact);

    private static string DisplayNameOf(ContactSnapshot contact) =>
        !string.IsNullOrWhiteSpace(contact.Petname) ? contact.Petname : contact.PublicDisplayName ?? "контакт";

    // --- both sides: follow the session state -------------------------------------------------------

    /// Called with every call-inbox refresh (next to the media sync): the owner's share and bar, the
    /// controller's automatic control, full screen leaving with the session.
    private void SyncRemoteControlSession()
    {
        CallSummary? call = currentCallInboxSnapshot?.CurrentCall;
        RemoteControlSession? session = call is not null && CallOverlayPresenter.IsLiveCallState(call) ? call.RemoteControl : null;

        if (session is null || call is null)
        {
            EndRemoteControlSessionUi();
            return;
        }

        if (!string.Equals(remoteControlSessionCallId, call.CallId, StringComparison.Ordinal))
        {
            remoteControlSessionCallId = call.CallId;
            remoteControlViewOnly = false;
        }

        if (session.IsOwner && session.IsApproved && CallOverlayPresenter.IsAnsweredCallState(call))
        {
            if (!ownerKeepsDisplayOn)
            {
                // Before the share: a sleeping display gives capture no frames at all.
                OwnerSessionPower.WakeDisplay();
                OwnerSessionPower.KeepDisplayOn(true);
                ownerKeepsDisplayOn = true;
            }

            EnsureOwnerSharesScreen(call);
            ShowRemoteControlBar(call);
            ReportOwnerSideState(call);
        }
        else
        {
            HideRemoteControlBar();
        }
    }

    private void EndRemoteControlSessionUi()
    {
        remoteControlSessionCallId = null;
        ownerShareStartedForCallId = null;
        lastReportedSharerState = null;
        HideRemoteControlBar();
        if (ownerKeepsDisplayOn)
        {
            OwnerSessionPower.KeepDisplayOn(false);
            ownerKeepsDisplayOn = false;
        }
        if (callFullScreen)
        {
            SetCallFullScreen(false);
        }
    }

    // --- owner ---------------------------------------------------------------------------------------

    /// Approved session: share the primary monitor (no system picker — the grant was the consent) and
    /// let this call's peer control it. Once per session; media must be up first.
    private void EnsureOwnerSharesScreen(CallSummary call)
    {
        if (string.Equals(ownerShareStartedForCallId, call.CallId, StringComparison.Ordinal) ||
            !callVideoService.IsActiveForDialog(call.DialogKey))
        {
            return;
        }

        ownerShareStartedForCallId = call.CallId;
        _ = StartOwnerScreenShareAsync(call);
    }

    /// On the UI thread, as every other screen share starts: the capture item and the video service
    /// belong to it. It used to run on the thread pool — the owner's share then failed with
    /// RPC_E_WRONG_THREAD every time, while this side showed «вами управляют» and the controller
    /// waited for a screen that never came (stand 2026-10-15).
    private async Task StartOwnerScreenShareAsync(CallSummary call)
    {
        string? failure = null;
        try
        {
            if (!callVideoService.IsScreenSharing)
            {
                await callVideoService.StartScreenShareAsync(ScreenShareCaptureSource.CreateForPrimaryMonitor());
            }
        }
        catch (Exception ex)
        {
            failure = NormalizeUserError(ex);
        }

        if (failure is not null || callVideoService.SharedMonitor is null)
        {
            AppTrace.Write("remote-control/owner", $"screen share failed: {failure ?? "no monitor"}");
            SetActionStatus($"Не удалось показать экран для удалённого управления: {failure ?? "монитор недоступен"}");
            return;
        }

        SetActionStatus(SetCallScreenControlGrant(true, call));
        RenderCurrentChatCallSummary();
    }

    /// Tell the controller, on change only, what stands in the way of its input on this side.
    private void ReportOwnerSideState(CallSummary call)
    {
        // Only once input is actually granted — a report before that is dropped by the core, and a
        // dropped "locked" would never be repeated.
        bool granting;
        try
        {
            granting = rustBridge.GetCallScreenControlState().IGrant;
        }
        catch (Exception)
        {
            granting = false;
        }

        if (!granting)
        {
            return;
        }

        string state = OwnerSessionPower.IsWorkstationLocked()
            ? "locked"
            : callVideoService.ScreenShareHasNoPicture ? "no_picture"
            : screenControlInjector?.OwnerHandActive == true ? "owner_active" : "ready";
        if (string.Equals(state, lastReportedSharerState, StringComparison.Ordinal) ||
            callVideoService.SharedMonitor is null)
        {
            return;
        }

        try
        {
            rustBridge.ReportCallScreenControlState(call.CallId, state);
            lastReportedSharerState = state;
        }
        catch (Exception ex)
        {
            AppTrace.WriteFailure("remote-control/owner", "report state", ex);
        }
    }

    /// Controller: the owner's side said why input may not land — say it over the screen.
    private void RenderRemoteControlNotice(CallSummary? call)
    {
        string? text = call?.RemoteControl is { IsController: true }
            ? DescribeSharerState(callScreenControlState.PeerState, MainWindowPresentation.BuildCallPartySummary(call.Participants))
            : null;
        // No picture yet: the identity card is what the user looks at, so the reason goes there in
        // place of "waiting for the screen" (the two used to contradict each other). Over a live
        // picture it is a pill on top.
        bool cardShown = CallIdentityPanel.Visibility == Visibility.Visible;
        if (text is not null && cardShown)
        {
            ChatCallStateTextBlock.Text = callScreenControlState.PeerState switch
            {
                "no_picture" => "Нет картинки",
                "locked" => "Компьютер заблокирован",
                _ => ChatCallStateTextBlock.Text,
            };
            ChatCallDetailsTextBlock.Text = text;
            RemoteControlNoticeBorder.Visibility = Visibility.Collapsed;
            return;
        }

        RemoteControlNoticeText.Text = text ?? string.Empty;
        RemoteControlNoticeBorder.Visibility = text is null ? Visibility.Collapsed : Visibility.Visible;
    }

    internal static string? DescribeSharerState(string? sharerState, string name) => sharerState switch
    {
        "locked" => $"Компьютер «{name}» заблокирован — управлять можно после разблокировки.",
        "owner_active" => $"«{name}» сам пользуется мышью — ваш ввод подождёт, пока он не остановится.",
        "no_picture" => $"Экран компьютера «{name}» не отдаёт картинку — похоже, монитор выключен. Управлять вслепую нельзя.",
        _ => null,
    };

    private void ShowRemoteControlBar(CallSummary call)
    {
        string name = MainWindowPresentation.BuildCallPartySummary(call.Participants);
        if (remoteControlBar is null)
        {
            remoteControlBar = new RemoteControlBar();
            remoteControlBar.StopRequested += OnRemoteControlBarStop;
        }

        remoteControlBar.Show($"«{name}» управляет этим компьютером", call.CallId);
    }

    private void HideRemoteControlBar()
    {
        remoteControlBar?.Hide();
    }

    private void OnRemoteControlBarStop(string callId)
    {
        try
        {
            rustBridge.HangUpCall(callId);
            SetActionStatus("Удалённое управление остановлено.");
        }
        catch (Exception ex)
        {
            SetActionStatus($"Не удалось остановить управление: {NormalizeUserError(ex)}");
        }

        RefreshCallInbox();
    }

    private void DisposeRemoteControlBar()
    {
        remoteControlBar?.Close();
        remoteControlBar = null;
    }

    /// A call (or a remote-control request) rings while the window is hidden in the background: bring
    /// it up once, so the owner can answer. Not on every refresh — that would steal focus forever.
    private void SurfaceWindowForIncomingCall(CallSummary? incoming)
    {
        if (incoming is null)
        {
            surfacedIncomingCallId = null;
            return;
        }

        if (string.Equals(surfacedIncomingCallId, incoming.CallId, StringComparison.Ordinal))
        {
            return;
        }

        surfacedIncomingCallId = incoming.CallId;
        if (!AppWindow.IsVisible)
        {
            ShowFromBackground();
        }
    }

    // --- controller: in the session -----------------------------------------------------------------

    /// Approved and the owner's input grant has arrived: control starts by itself — the user asked
    /// for it by pressing "Управлять компьютером". "Только смотреть" stops it until pressed again.
    private void AutoStartRemoteControl(bool mayControl)
    {
        if (currentChatCallSummary?.RemoteControl is not { IsController: true, IsApproved: true } ||
            !mayControl || remoteControlViewOnly || screenControlViewer is not null)
        {
            return;
        }

        StartRemoteScreenControl(currentChatCallSummary.DialogKey, sharer: null, RemoteScreenControlSurface);
        SetActionStatus("Вы управляете компьютером. Мышь и клавиатура над экраном уходят ему; «Только смотреть» — пауза.");
    }

    private void OnToggleCallFullScreenClicked(object sender, RoutedEventArgs e) => SetCallFullScreen(!callFullScreen);

    private void SetCallFullScreen(bool on)
    {
        try
        {
            AppWindow.SetPresenter(on ? AppWindowPresenterKind.FullScreen : AppWindowPresenterKind.Default);
            callFullScreen = on;
        }
        catch (Exception ex)
        {
            AppTrace.WriteFailure("remote-control/fullscreen", "set presenter", ex);
            callFullScreen = false;
        }

        ToggleCallFullScreenIcon.Glyph = ((char)(callFullScreen ? 0xE73F : 0xE740)).ToString();
        ToggleCallFullScreenText.Text = callFullScreen ? "Выйти из полноэкранного" : "Во весь экран";
    }
}
