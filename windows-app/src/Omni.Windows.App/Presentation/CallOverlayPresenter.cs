using System;
using Omni.Windows.App.Core;

namespace Omni.Windows.App.Presentation;

// Pure presentation logic for the call overlay (docs/architecture/ui-audit.md #6/#7).
// Computes the entire overlay view-state from inputs so MainWindow only applies it
// to controls. No UI / no service dependencies → unit-testable
// (see CallOverlayPresenterTests). Shared call-state classification predicates also
// live here (used by the banner / inbox / mute-toggle / refresh paths).
internal sealed record CallOverlayState(
    bool Visible,
    // The one status line under the name: "Ждём ответа · 00:04", "12:31" once talking,
    // "Восстанавливаем связь · 12:31". (It used to be three lines — state, timer and a sentence
    // retelling both: "Разговор идёт / 00:09 / Разговор с PC уже идёт.")
    string StateText,
    string PeerName,
    string AvatarText,
    string DetailsText,
    string TimerText,
    string NetworkText,
    bool IdentityVisible,
    bool VideoGridVisible,
    bool RemotePlaceholderVisible,
    bool AcceptVisible,
    bool DeclineVisible,
    bool MuteVisible,
    bool Muted,
    string MuteAccessibleName,
    bool HangUpVisible,
    // Audio-first: the camera is an opt-in toggle. CameraToggleVisible mirrors mute
    // (only on a connected call); CameraOn reflects whether we are actually sending video.
    bool CameraToggleVisible,
    bool CameraOn,
    string CameraToggleAccessibleName,
    // Remote-control session (docs/decisions/remote-access-for-contacts.md): no microphone, no
    // camera, no screen-share button; the red button ends the session; the controller can go full
    // screen.
    bool RemoteControl = false,
    string HangUpAccessibleName = "Завершить звонок",
    bool FullScreenVisible = false,
    // An incoming ring offers a second answer: pick up with the microphone already off, so not a
    // moment of the room goes out (the plain answer opens the mic and the mute comes a second later).
    bool AcceptMutedVisible = false,
    // The microphone is on but hears nothing at all (only digital zeros): which one, and where to
    // pick another. Empty when it hears, is muted, or there is no live call.
    string MicWarning = "");

internal static class CallOverlayPresenter
{
    public static bool IsLiveCallState(string? state)
        => (state ?? string.Empty).Trim().ToLowerInvariant() is
            "calling" or "ringing" or "connecting" or "active" or "reconnecting";

    public static bool IsLiveCallState(CallSummary summary)
        => IsLiveCallState(summary.State);

    /// <summary>
    /// The call is ANSWERED — the only state in which media (microphone included) may run.
    /// `connecting` is an outgoing call still waiting for CallAccept, and a call nobody picks up
    /// slides from `connecting` into `reconnecting` on the liveness watchdog without ever having
    /// been answered — so neither counts. See StartCallMediaWhenCallIsAnswered.
    /// </summary>
    public static bool IsAnsweredCallState(string? state)
        => string.Equals((state ?? string.Empty).Trim(), "active", StringComparison.OrdinalIgnoreCase);

    public static bool IsAnsweredCallState(CallSummary summary)
        => IsAnsweredCallState(summary.State);

    public static bool IsIncomingRingingCall(CallSummary summary)
        => string.Equals(summary.Direction, "incoming", StringComparison.OrdinalIgnoreCase) &&
           string.Equals(summary.State, "ringing", StringComparison.OrdinalIgnoreCase);

    /// <summary>
    /// Mute (and the camera toggle, which rides on the same predicate) is offered exactly while
    /// media can be running: an answered call, plus the reconnect window of one. It used to
    /// include `connecting` — an outgoing call still waiting to be picked up — where there is no
    /// audio to mute and no peer to send a frame to, so the buttons promised something they
    /// couldn't do.
    /// </summary>
    public static bool IsMuteSupported(CallSummary summary)
        => (summary.State ?? string.Empty).Trim().ToLowerInvariant() is
            "active" or "reconnecting";

    /// <summary>
    /// The microphone, unlike the camera, can be switched off before the other side picks up — an
    /// outgoing call that is still ringing: the choice is kept in the core's call summary and the
    /// media opens muted (CallVideoService.StartAsync). Not on an incoming ring: there the answer
    /// is the first thing to do.
    /// </summary>
    public static bool IsMicToggleSupported(CallSummary summary)
        => IsMuteSupported(summary) ||
           (IsLiveCallState(summary) && !IsIncomingRingingCall(summary));

    public static bool IsHangUpSupported(CallSummary summary)
        => IsLiveCallState(summary);

    public static bool ShouldRenderCallCard(CallSummary summary)
        => IsLiveCallState(summary) || IsIncomingRingingCall(summary);

    // --- Call selection (which call drives the hot-refresh cadence / incoming banner /
    // current-chat actions). Pure functions of the inbox snapshot + the current dialog. ---

    public static bool ShouldUseHotCallRefresh(CallInboxSnapshot? inbox)
    {
        if (inbox?.CurrentCall is CallSummary currentCall && IsLiveCallState(currentCall))
        {
            return true;
        }
        if (inbox is null)
        {
            return false;
        }
        foreach (CallSummary incoming in inbox.IncomingCalls)
        {
            if (IsLiveCallState(incoming))
            {
                return true;
            }
        }
        return false;
    }

    public static CallSummary? ResolveDisplayedChatCallSummary(
        CallInboxSnapshot? inbox, string? dialogKey, CallSummary? headerCallSummary)
    {
        if (string.IsNullOrWhiteSpace(dialogKey))
        {
            return null;
        }
        if (inbox?.CurrentCall is CallSummary currentCall &&
            string.Equals(currentCall.DialogKey, dialogKey, StringComparison.Ordinal))
        {
            return currentCall;
        }
        if (inbox is not null)
        {
            foreach (CallSummary incoming in inbox.IncomingCalls)
            {
                if (string.Equals(incoming.DialogKey, dialogKey, StringComparison.Ordinal))
                {
                    return incoming;
                }
            }
        }
        if (headerCallSummary is not null &&
            string.Equals(headerCallSummary.DialogKey, dialogKey, StringComparison.Ordinal))
        {
            return headerCallSummary;
        }
        return null;
    }

    public static CallSummary? GetIncomingBannerCall(CallInboxSnapshot? inbox)
    {
        if (inbox?.CurrentCall is CallSummary currentCall && IsIncomingRingingCall(currentCall))
        {
            return currentCall;
        }
        if (inbox is null)
        {
            return null;
        }
        foreach (CallSummary incoming in inbox.IncomingCalls)
        {
            if (IsIncomingRingingCall(incoming))
            {
                return incoming;
            }
        }
        return null;
    }

    public static CallSummary? GetActionableCurrentChatCall(
        CallInboxSnapshot? inbox, CallSummary? currentChatCallSummary, string? dialogKey)
    {
        if (!string.IsNullOrWhiteSpace(dialogKey) &&
            currentChatCallSummary is not null &&
            string.Equals(currentChatCallSummary.DialogKey, dialogKey, StringComparison.Ordinal) &&
            IsLiveCallState(currentChatCallSummary))
        {
            return currentChatCallSummary;
        }
        if (inbox?.CurrentCall is CallSummary currentCall && IsLiveCallState(currentCall))
        {
            return currentCall;
        }
        return null;
    }

    // summary: current chat call (may be null); videoActive: media live for this dialog;
    // hasRemoteVideo: a remote frame has arrived; localCameraActive: we are sending video.
    public static CallOverlayState Compute(
        CallSummary? summary,
        bool videoActive,
        bool hasRemoteVideo,
        bool localCameraActive,
        string? silentMicrophone = null)
    {
        if (summary is not null && ShouldRenderCallCard(summary) && summary.RemoteControl is { } session)
        {
            return ComputeRemoteControl(summary, session, videoActive, hasRemoteVideo);
        }

        if (summary is not null && ShouldRenderCallCard(summary))
        {
            bool incomingRinging = IsIncomingRingingCall(summary);
            string peerName = MainWindowPresentation.BuildCallPartySummary(summary.Participants);
            return new CallOverlayState(
                Visible: true,
                StateText: BuildStatusLine(summary),
                PeerName: peerName,
                AvatarText: BuildAvatarInitial(peerName),
                DetailsText: string.Empty,
                TimerText: BuildTimer(summary),
                // The network is a fact of a running call; while it rings there is none to rate
                // ("Сеть: восстанавливается" under "Ждём ответа" read as a fault).
                NetworkText: IsMuteSupported(summary) ? BuildNetwork(summary) : string.Empty,
                // The avatar/name card is the audio-call screen; hide it only once a
                // real remote video frame is filling the surface.
                IdentityVisible: !hasRemoteVideo,
                VideoGridVisible: videoActive,
                RemotePlaceholderVisible: !hasRemoteVideo,
                AcceptVisible: incomingRinging,
                DeclineVisible: incomingRinging,
                MuteVisible: IsMicToggleSupported(summary),
                Muted: summary.LocalMicMuted,
                MuteAccessibleName: summary.LocalMicMuted ? "Включить микрофон" : "Выключить микрофон",
                // Incoming ringing shows Accept + Decline (Decline is the red "reject"); the red
                // "Завершить"/hang-up belongs to a live call, not the ringing prompt. Without the
                // !incomingRinging guard, ringing ∈ IsLiveCallState → both Decline and HangUp render,
                // i.e. two red buttons. See docs/bugs/live-test-audit-2026-07-18.md (#4).
                HangUpVisible: IsLiveCallState(summary) && !incomingRinging,
                CameraToggleVisible: IsMuteSupported(summary),
                CameraOn: localCameraActive,
                CameraToggleAccessibleName: localCameraActive ? "Выключить камеру" : "Включить камеру",
                AcceptMutedVisible: incomingRinging,
                MicWarning: BuildMicWarning(summary, silentMicrophone));
        }

        if (videoActive)
        {
            return new CallOverlayState(
                Visible: true,
                StateText: "Видеозвонок",
                PeerName: "Собеседник",
                AvatarText: "?",
                DetailsText: localCameraActive
                    ? "Камера отправляется внутри защищённой сессии."
                    : "Получаем видео внутри защищённой сессии.",
                TimerText: string.Empty,
                NetworkText: string.Empty,
                IdentityVisible: !hasRemoteVideo,
                VideoGridVisible: true,
                RemotePlaceholderVisible: !hasRemoteVideo,
                AcceptVisible: false,
                DeclineVisible: false,
                MuteVisible: false,
                Muted: false,
                MuteAccessibleName: "Выключить микрофон",
                HangUpVisible: true,
                CameraToggleVisible: true,
                CameraOn: localCameraActive,
                CameraToggleAccessibleName: localCameraActive ? "Выключить камеру" : "Включить камеру");
        }

        return new CallOverlayState(
            Visible: false,
            StateText: "Звонок недоступен",
            PeerName: string.Empty,
            AvatarText: "?",
            DetailsText: string.Empty,
            TimerText: string.Empty,
            NetworkText: string.Empty,
            IdentityVisible: false,
            VideoGridVisible: false,
            RemotePlaceholderVisible: true,
            AcceptVisible: false,
            DeclineVisible: false,
            MuteVisible: false,
            Muted: false,
            MuteAccessibleName: "Выключить микрофон",
            HangUpVisible: false,
            CameraToggleVisible: false,
            CameraOn: false,
            CameraToggleAccessibleName: "Включить камеру");
    }

    /// <summary>
    /// The call panel during a remote-control session. Controller: the peer's screen fills it; until
    /// it does, the state line says honestly what we wait for. Owner: there is nothing to watch — our
    /// own screen is what is shared — so the panel says who controls this computer, with "Остановить".
    /// </summary>
    internal static CallOverlayState ComputeRemoteControl(
        CallSummary summary,
        RemoteControlSession session,
        bool videoActive,
        bool hasRemoteVideo)
    {
        string peerName = MainWindowPresentation.BuildCallPartySummary(summary.Participants);
        bool incomingRinging = IsIncomingRingingCall(summary);
        string state = (summary.State ?? string.Empty).Trim().ToLowerInvariant();
        (string stateText, string details) = session.IsOwner
            ? state switch
            {
                "ringing" => ($"{peerName} хочет управлять вашим компьютером",
                    "Разрешить на этот раз или отклонить. Без ответа запрос отклонится сам."),
                "reconnecting" => ("Связь прервалась", "Пробуем восстановить сеанс управления."),
                _ => ($"{peerName} управляет этим компьютером",
                    "Видит весь основной экран, управляет мышью и клавиатурой. Ваша мышь важнее: пока вы ей двигаете, чужой ввод ждёт."),
            }
            : state switch
            {
                "connecting" or "calling" => ($"Подключаемся к компьютеру {peerName}…", "Компьютер решает, пустить ли вас."),
                "ringing" => ($"Ждём ответа {peerName}…", "Владелец компьютера должен разрешить подключение."),
                "reconnecting" => ("Связь прервалась", "Пробуем восстановить сеанс."),
                _ when !hasRemoteVideo => ($"Ждём экран {peerName}…", "Владелец начинает показ экрана."),
                _ => ($"Компьютер {peerName}", string.Empty),
            };
        bool live = IsMuteSupported(summary); // active or reconnecting
        bool controllerWatching = session.IsController && live && hasRemoteVideo;
        return new CallOverlayState(
            Visible: true,
            StateText: stateText,
            PeerName: peerName,
            AvatarText: BuildAvatarInitial(peerName),
            DetailsText: details,
            TimerText: BuildTimer(summary),
            NetworkText: BuildNetwork(summary),
            IdentityVisible: !controllerWatching,
            // Owner: our own screen is the picture — never show it back to us in the panel.
            VideoGridVisible: session.IsController && videoActive,
            RemotePlaceholderVisible: !hasRemoteVideo,
            AcceptVisible: incomingRinging,
            DeclineVisible: incomingRinging,
            MuteVisible: false,
            Muted: true,
            MuteAccessibleName: "Микрофон не используется",
            HangUpVisible: IsLiveCallState(summary) && !incomingRinging,
            CameraToggleVisible: false,
            CameraOn: false,
            CameraToggleAccessibleName: "Камера не используется",
            RemoteControl: true,
            HangUpAccessibleName: session.IsOwner ? "Остановить управление" : "Завершить сеанс",
            FullScreenVisible: controllerWatching);
    }

    // First letter of the peer's display name, for the call avatar fallback.
    /// <summary>
    /// "The other side hears nothing from us" — said only while the mic is on in an answered call;
    /// a muted mic is silent on purpose.
    /// </summary>
    internal static string BuildMicWarning(CallSummary summary, string? silentMicrophone)
        => string.IsNullOrWhiteSpace(silentMicrophone) || !IsMicToggleSupported(summary) || summary.LocalMicMuted
            ? string.Empty
            // Windows names it "Микрофон (…)" already; the device name stands alone.
            : $"«{silentMicrophone.Trim()}» ничего не слышит — вас не слышно. Другой микрофон: Настройки → Звонки.";

    private static string BuildAvatarInitial(string? peerName)
    {
        if (!string.IsNullOrWhiteSpace(peerName))
        {
            foreach (char c in peerName)
            {
                if (char.IsLetterOrDigit(c))
                {
                    return char.ToUpperInvariant(c).ToString();
                }
            }
        }

        return "?";
    }

    internal static string BuildStatusLine(CallSummary summary)
    {
        string timer = BuildTimer(summary);
        return (summary.State ?? string.Empty).Trim().ToLowerInvariant() switch
        {
            "ringing" when string.Equals(summary.Direction, "incoming", StringComparison.OrdinalIgnoreCase) => "Входящий звонок",
            "calling" => $"Звоним · {timer}",
            "ringing" => $"Ждём ответа · {timer}",
            // An outgoing call waits here for the answer too; "Подключаем медиа" was our word.
            "connecting" => $"Соединяем · {timer}",
            "active" => timer,
            "reconnecting" => $"Восстанавливаем связь · {timer}",
            _ => MainWindowPresentation.MapCallState(summary.State ?? string.Empty, summary.Direction ?? string.Empty),
        };
    }

    private static string BuildTimer(CallSummary summary)
    {
        if (summary.ConnectedAtUnixMs.HasValue || summary.EndedAtUnixMs.HasValue)
        {
            return MainWindowPresentation.BuildCallDurationText(
                summary.StartedAtUnixMs,
                summary.ConnectedAtUnixMs,
                summary.EndedAtUnixMs);
        }

        return MainWindowPresentation.BuildCallDurationText(summary.StartedAtUnixMs, null, null);
    }

    private static string BuildNetwork(CallSummary summary)
    {
        if (string.Equals(summary.State, "reconnecting", StringComparison.OrdinalIgnoreCase) &&
            summary.ReconnectDeadlineUnixMs.HasValue)
        {
            DateTimeOffset reconnectDeadline = DateTimeOffset
                .FromUnixTimeMilliseconds(summary.ReconnectDeadlineUnixMs.Value)
                .ToLocalTime();
            return $"Восстанавливаем до {reconnectDeadline:HH:mm:ss}";
        }

        return MainWindowPresentation.MapCallNetworkQuality(summary.NetworkQuality);
    }
}
