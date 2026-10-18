using System.Collections.Generic;
using Omni.Windows.App;
using Omni.Windows.App.Core;
using Omni.Windows.App.Presentation;
using Xunit;

namespace Omni.Windows.App.Tests;

// Remote-control sessions (docs/decisions/remote-access-for-contacts.md), shell side.
public sealed class RemoteControlSessionTests
{
    private static CallSummary Session(string state, string direction, string role, string phase) => new(
        CallId: "c1",
        DialogKey: "d1",
        ContactId: "x1",
        Direction: direction,
        State: state,
        Outcome: null,
        DeclineReason: null,
        Participants: new List<CallParticipantSummary> { new("p1", "a1", "dev1", "Аня", IsLocal: false, IsMuted: false) },
        LocalMicMuted: false,
        NetworkQuality: "good",
        StartedAtUnixMs: 0,
        RingingStartedAtUnixMs: null,
        ConnectedAtUnixMs: state == "active" ? 1 : null,
        EndedAtUnixMs: null,
        ReconnectDeadlineUnixMs: null,
        UpdatedAtUnixMs: 0,
        RemoteControl: new RemoteControlSession(role, phase, null));

    [Fact]
    public void A_session_never_offers_microphone_camera_or_screen_share()
    {
        CallOverlayState controller = CallOverlayPresenter.Compute(
            Session("active", "outgoing", "controller", "approved"), videoActive: true, hasRemoteVideo: true, localCameraActive: false);
        Assert.True(controller.RemoteControl);
        Assert.False(controller.MuteVisible);
        Assert.False(controller.CameraToggleVisible);
        Assert.True(controller.FullScreenVisible, "the controller watching the screen may go full screen");
        Assert.Equal("Завершить сеанс", controller.HangUpAccessibleName);

        // Negative half: an ordinary call keeps its microphone and camera, and no full-screen button.
        CallSummary ordinary = Session("active", "outgoing", "controller", "approved") with { RemoteControl = null };
        CallOverlayState call = CallOverlayPresenter.Compute(ordinary, videoActive: true, hasRemoteVideo: true, localCameraActive: false);
        Assert.True(call.MuteVisible);
        Assert.False(call.FullScreenVisible);
    }

    [Fact]
    public void Each_wait_is_named_for_what_it_waits_on()
    {
        Assert.Contains("Ждём ответа", CallOverlayPresenter.Compute(
            Session("ringing", "outgoing", "controller", "awaiting_owner"), false, false, false).StateText);
        Assert.Contains("Ждём экран", CallOverlayPresenter.Compute(
            Session("active", "outgoing", "controller", "approved"), true, hasRemoteVideo: false, false).StateText);

        CallOverlayState asked = CallOverlayPresenter.Compute(
            Session("ringing", "incoming", "owner", "awaiting_owner"), false, false, false);
        Assert.Contains("хочет управлять", asked.StateText);
        Assert.True(asked.AcceptVisible);
        Assert.False(asked.AcceptMutedVisible); // a control request has no microphone to keep off

        CallOverlayState owned = CallOverlayPresenter.Compute(
            Session("active", "incoming", "owner", "approved"), videoActive: true, hasRemoteVideo: false, false);
        Assert.Contains("управляет этим компьютером", owned.StateText);
        Assert.Equal("Остановить управление", owned.HangUpAccessibleName);
        // Negative half: the owner is never shown its own screen back, and has nothing to go full screen for.
        Assert.False(owned.VideoGridVisible);
        Assert.False(owned.FullScreenVisible);
    }

    [Fact]
    public void Failures_are_told_in_the_users_terms()
    {
        Assert.Contains("не в сети", MainWindow.DescribeRemoteControlFailure("no live path to the peer: the call was not placed", "Аня"));
        Assert.Contains("в звонке", MainWindow.DescribeRemoteControlFailure("a call with this contact is already running: ...", "Аня"));
        Assert.Contains("не разрешал", MainWindow.DescribeRemoteControlFailure("this contact has not allowed you to control their computer", "Аня"));
        // Negative half: an unknown reason is passed through, not dressed up as "offline".
        string other = MainWindow.DescribeRemoteControlFailure("disk full", "Аня");
        Assert.Contains("disk full", other);
        Assert.DoesNotContain("не в сети", other);
    }

    [Fact]
    public void The_chat_line_names_a_session_and_its_span()
    {
        var ran = new CallHistoryEvent("c1", "outgoing", "completed", null, new List<CallParticipantSummary>(),
            StartedAtUnixMs: 0, ConnectedAtUnixMs: 60_000, EndedAtUnixMs: 120_000, DurationSeconds: 60, RemoteControl: true);
        Assert.StartsWith("Сеанс управления ", MainWindowPresentation.BuildCallHistorySummary(ran));
        Assert.Contains("–", MainWindowPresentation.BuildCallHistorySummary(ran));

        var refused = ran with { Outcome = "declined", DeclineReason = "security_blocked", ConnectedAtUnixMs = null };
        Assert.Equal("Сеанс управления: доступ не выдан", MainWindowPresentation.BuildCallHistorySummary(refused));

        // Negative half: an ordinary call is not called a session.
        Assert.DoesNotContain("Сеанс управления", MainWindowPresentation.BuildCallHistorySummary(ran with { RemoteControl = false }));
    }
    [Fact]
    public void The_owners_side_is_said_not_guessed()
    {
        Assert.Contains("заблокирован", MainWindow.DescribeSharerState("locked", "ПК"));
        Assert.Contains("сам пользуется мышью", MainWindow.DescribeSharerState("owner_active", "ПК"));
        // Negative half: ready, or nothing reported, puts nothing over the screen.
        Assert.Null(MainWindow.DescribeSharerState("ready", "ПК"));
        Assert.Null(MainWindow.DescribeSharerState(null, "ПК"));
    }
}
