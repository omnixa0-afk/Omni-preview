using System.Collections.Generic;
using Omni.Windows.App.Core;
using Omni.Windows.App.Presentation;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class CallOverlayPresenterTests
{
    private static CallSummary Call(
        string state,
        string direction = "outgoing",
        bool muted = false)
        => new CallSummary(
            CallId: "c1",
            DialogKey: "d1",
            ContactId: "x1",
            Direction: direction,
            State: state,
            Outcome: null,
            DeclineReason: null,
            Participants: new List<CallParticipantSummary>
            {
                new("p1", "a1", "dev1", "Алиса", IsLocal: false, IsMuted: false),
            },
            LocalMicMuted: muted,
            NetworkQuality: "good",
            StartedAtUnixMs: 0,
            RingingStartedAtUnixMs: null,
            ConnectedAtUnixMs: null,
            EndedAtUnixMs: null,
            ReconnectDeadlineUnixMs: null,
            UpdatedAtUnixMs: 0);

    /// A microphone that hears nothing at all is said out loud in an answered call — and only there:
    /// not while it rings, not when we muted it on purpose.
    [Fact]
    public void ASilentMicrophoneIsNamedOnlyWhileItShouldBeHeard()
    {
        CallOverlayState active = CallOverlayPresenter.Compute(
            Call("active"), videoActive: true, hasRemoteVideo: false, localCameraActive: false,
            silentMicrophone: "Steam Streaming Microphone");
        Assert.Contains("Steam Streaming Microphone", active.MicWarning);
        Assert.Contains("Настройки → Звонки", active.MicWarning);

        Assert.Equal(string.Empty, CallOverlayPresenter.Compute(
            Call("active", muted: true), true, false, false, silentMicrophone: "Steam Streaming Microphone").MicWarning);
        Assert.Equal(string.Empty, CallOverlayPresenter.Compute(
            Call("ringing", direction: "incoming"), false, false, false, silentMicrophone: "Steam Streaming Microphone").MicWarning);
        Assert.Equal(string.Empty, CallOverlayPresenter.Compute(
            Call("active"), true, false, false, silentMicrophone: null).MicWarning);
    }

    [Fact]
    public void NoCall_NoVideo_OverlayHidden()
    {
        CallOverlayState state = CallOverlayPresenter.Compute(null, videoActive: false, hasRemoteVideo: false, localCameraActive: false);

        Assert.False(state.Visible);
        Assert.False(state.MuteVisible);
        Assert.False(state.HangUpVisible);
    }

    [Fact]
    public void VideoActive_WithoutCallSummary_ShowsVideoSurface()
    {
        CallOverlayState state = CallOverlayPresenter.Compute(null, videoActive: true, hasRemoteVideo: false, localCameraActive: true);

        Assert.True(state.Visible);
        Assert.Equal("Видеозвонок", state.StateText);
        Assert.True(state.VideoGridVisible);
        Assert.True(state.RemotePlaceholderVisible); // no remote frame yet
        Assert.True(state.HangUpVisible);
        Assert.False(state.AcceptVisible);
        Assert.False(state.MuteVisible);
    }

    [Fact]
    public void ActiveCall_ShowsMuteAndHangUp_NoAcceptDecline()
    {
        CallOverlayState state = CallOverlayPresenter.Compute(Call("active"), videoActive: true, hasRemoteVideo: true, localCameraActive: true);

        Assert.True(state.Visible);
        Assert.True(state.MuteVisible);
        Assert.True(state.HangUpVisible);
        Assert.False(state.AcceptVisible);
        Assert.False(state.AcceptMutedVisible);
        Assert.False(state.DeclineVisible);
        Assert.False(state.RemotePlaceholderVisible); // remote frame present
    }

    [Fact]
    public void AudioCall_ShowsIdentityCard_HiddenOnceRemoteVideoArrives()
    {
        CallOverlayState audio = CallOverlayPresenter.Compute(Call("active"), videoActive: false, hasRemoteVideo: false, localCameraActive: false);
        Assert.True(audio.IdentityVisible);            // avatar+name card is the audio-call screen
        Assert.False(audio.VideoGridVisible);
        Assert.False(string.IsNullOrWhiteSpace(audio.PeerName));
        Assert.False(string.IsNullOrWhiteSpace(audio.AvatarText));

        CallOverlayState remoteVideo = CallOverlayPresenter.Compute(Call("active"), videoActive: true, hasRemoteVideo: true, localCameraActive: true);
        Assert.False(remoteVideo.IdentityVisible);     // remote video fills the surface instead
    }

    [Fact]
    public void AudioFirstCall_CameraToggleVisible_OffByDefault_OnWhenCameraActive()
    {
        // A connected call starts audio-only: the camera toggle is offered but off.
        CallOverlayState audioFirst = CallOverlayPresenter.Compute(Call("active"), videoActive: false, hasRemoteVideo: false, localCameraActive: false);
        Assert.True(audioFirst.CameraToggleVisible);
        Assert.False(audioFirst.CameraOn);
        Assert.Equal("Включить камеру", audioFirst.CameraToggleAccessibleName);

        // Once the user turns the camera on, the toggle reflects the live state.
        CallOverlayState withCamera = CallOverlayPresenter.Compute(Call("active"), videoActive: true, hasRemoteVideo: false, localCameraActive: true);
        Assert.True(withCamera.CameraToggleVisible);
        Assert.True(withCamera.CameraOn);
        Assert.Equal("Выключить камеру", withCamera.CameraToggleAccessibleName);
    }

    [Fact]
    public void RingingCall_CameraToggleHidden_UntilConnected()
    {
        // Camera toggle mirrors mute: not actionable before the call connects.
        CallOverlayState ringing = CallOverlayPresenter.Compute(Call("ringing", "incoming"), videoActive: false, hasRemoteVideo: false, localCameraActive: false);
        Assert.False(ringing.CameraToggleVisible);
        Assert.False(ringing.MuteVisible);
    }

    [Fact]
    public void IncomingRinging_ShowsAcceptDecline_NotHangUp_AndNotMutable()
    {
        CallOverlayState state = CallOverlayPresenter.Compute(Call("ringing", "incoming"), videoActive: false, hasRemoteVideo: false, localCameraActive: false);

        Assert.True(state.Visible);
        Assert.True(state.AcceptVisible);
        Assert.True(state.AcceptMutedVisible); // "Без микрофона": answer with the mic already off
        Assert.True(state.DeclineVisible);
        // Incoming ringing offers Accept + Decline only; the red "Завершить"/hang-up belongs to a
        // live call, not the ringing prompt. Showing both Decline and HangUp gave two red buttons
        // (docs/bugs/live-test-audit-2026-07-18.md #4). ringing ∈ IsLiveCallState, but HangUp is
        // now guarded by !incomingRinging.
        Assert.False(state.HangUpVisible);
        Assert.False(state.MuteVisible);    // mute only when connecting/active/reconnecting
    }

    [Fact]
    public void MutedCall_ReflectsMuteStateAndAccessibleName()
    {
        CallOverlayState muted = CallOverlayPresenter.Compute(Call("active", muted: true), videoActive: false, hasRemoteVideo: false, localCameraActive: false);
        CallOverlayState unmuted = CallOverlayPresenter.Compute(Call("active", muted: false), videoActive: false, hasRemoteVideo: false, localCameraActive: false);

        Assert.True(muted.Muted);
        Assert.Equal("Включить микрофон", muted.MuteAccessibleName);
        Assert.False(unmuted.Muted);
        Assert.Equal("Выключить микрофон", unmuted.MuteAccessibleName);
    }

    [Theory]
    [InlineData("calling", true)]
    [InlineData("ringing", true)]
    [InlineData("connecting", true)]
    [InlineData("active", true)]
    [InlineData("reconnecting", true)]
    [InlineData("ended", false)]
    [InlineData("declined", false)]
    [InlineData(null, false)]
    public void IsLiveCallState_ClassifiesStates(string? callState, bool expected)
        => Assert.Equal(expected, CallOverlayPresenter.IsLiveCallState(callState));

    [Theory]
    [InlineData("active", true)]
    [InlineData("reconnecting", true)]
    // An outgoing call waiting to be picked up has no audio to mute and no peer to send to.
    [InlineData("connecting", false)]
    [InlineData("ringing", false)]
    [InlineData("calling", false)]
    [InlineData("ended", false)]
    public void IsMuteSupported_OnlyWhileMediaCanRun(string callState, bool expected)
        => Assert.Equal(expected, CallOverlayPresenter.IsMuteSupported(Call(callState)));

    // The mic, unlike the camera, can be switched off while our call still rings (it opens muted);
    // an incoming ring offers only accept/decline.
    [Theory]
    [InlineData("active", "outgoing", true)]
    [InlineData("reconnecting", "incoming", true)]
    [InlineData("connecting", "outgoing", true)]
    [InlineData("ringing", "outgoing", true)]
    [InlineData("ringing", "incoming", false)]
    [InlineData("ended", "outgoing", false)]
    public void IsMicToggleSupported_IncludesOurRingingCall(string callState, string direction, bool expected)
    {
        CallSummary call = Call(callState) with { Direction = direction };
        Assert.Equal(expected, CallOverlayPresenter.IsMicToggleSupported(call));
    }

    // The microphone may only open on an ANSWERED call. `connecting` is an outgoing call still
    // ringing on the other end, and the liveness watchdog turns an unanswered one into
    // `reconnecting` — neither ever reached a peer that agreed to media.
    [Theory]
    [InlineData("active", true)]
    [InlineData("connecting", false)]
    [InlineData("reconnecting", false)]
    [InlineData("ringing", false)]
    [InlineData("calling", false)]
    [InlineData("ended", false)]
    [InlineData(null, false)]
    public void IsAnsweredCallState_OnlyActive(string? callState, bool expected)
        => Assert.Equal(expected, CallOverlayPresenter.IsAnsweredCallState(callState));

    private static CallInboxSnapshot Inbox(CallSummary? current, params CallSummary[] incoming)
        => new CallInboxSnapshot(current, incoming, 0);

    [Fact]
    public void GetIncomingBannerCall_ReturnsRingingIncoming_FromCurrentOrIncoming()
    {
        CallSummary ringing = Call("ringing", "incoming");
        Assert.Same(ringing, CallOverlayPresenter.GetIncomingBannerCall(Inbox(ringing)));
        Assert.Same(ringing, CallOverlayPresenter.GetIncomingBannerCall(Inbox(null, ringing)));
        Assert.Null(CallOverlayPresenter.GetIncomingBannerCall(Inbox(Call("active", "incoming"))));
        Assert.Null(CallOverlayPresenter.GetIncomingBannerCall(null));
    }

    [Fact]
    public void ShouldUseHotCallRefresh_TrueWhenAnyLiveCall()
    {
        Assert.True(CallOverlayPresenter.ShouldUseHotCallRefresh(Inbox(Call("active"))));
        Assert.True(CallOverlayPresenter.ShouldUseHotCallRefresh(Inbox(null, Call("ringing", "incoming"))));
        Assert.False(CallOverlayPresenter.ShouldUseHotCallRefresh(Inbox(Call("ended"))));
        Assert.False(CallOverlayPresenter.ShouldUseHotCallRefresh(null));
    }

    [Fact]
    public void GetActionableCurrentChatCall_PrefersMatchingChatSummary_ThenInboxCurrent()
    {
        CallSummary chat = Call("active");   // DialogKey "d1"
        CallSummary inboxCurrent = Call("active");
        Assert.Same(chat, CallOverlayPresenter.GetActionableCurrentChatCall(Inbox(inboxCurrent), chat, "d1"));
        Assert.Same(inboxCurrent, CallOverlayPresenter.GetActionableCurrentChatCall(Inbox(inboxCurrent), null, "d1"));
        Assert.Null(CallOverlayPresenter.GetActionableCurrentChatCall(Inbox(Call("ended")), null, "d1"));
    }

    [Fact]
    public void ResolveDisplayedChatCallSummary_MatchesDialogKey()
    {
        CallSummary current = Call("active");  // DialogKey "d1"
        Assert.Same(current, CallOverlayPresenter.ResolveDisplayedChatCallSummary(Inbox(current), "d1", null));
        Assert.Null(CallOverlayPresenter.ResolveDisplayedChatCallSummary(Inbox(current), "other-dialog", null));
        Assert.Null(CallOverlayPresenter.ResolveDisplayedChatCallSummary(Inbox(current), null, null));
    }
}
