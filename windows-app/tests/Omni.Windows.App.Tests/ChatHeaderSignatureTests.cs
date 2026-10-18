using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Chats;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class ChatHeaderSignatureTests
{
    [Fact]
    public void Build_IncludesHeaderSecuritySessionAndCallFields_AndNoRouteFields()
    {
        string signature = ChatHeaderSignature.Build(
            CreateHeader(contactCard: CreateContactCard()),
            CreateSecuritySummary(),
            CreateSessionSummary(),
            CreateCallSummary());

        Assert.Contains("dialog:instance:peer", signature, StringComparison.Ordinal);
        Assert.Contains("contact-1", signature, StringComparison.Ordinal);
        Assert.Contains("verified", signature, StringComparison.Ordinal);
        Assert.Contains("connected", signature, StringComparison.Ordinal);
        Assert.Contains("call-1", signature, StringComparison.Ordinal);
        Assert.Contains("participant-remote>account-peer>device-peer>Peer>False>True", signature, StringComparison.Ordinal);
        // The pre-mesh direct-route summary is gone from the core, so nothing route-shaped feeds the header.
        Assert.DoesNotContain("internet_direct", signature, StringComparison.Ordinal);
        Assert.DoesNotContain("no-diagnostics", signature, StringComparison.Ordinal);
    }

    [Fact]
    public void Build_UsesStableSentinelsForMissingOptionalDomains()
    {
        string signature = ChatHeaderSignature.Build(
            CreateHeader(contactCard: null),
            CreateSecuritySummary(),
            CreateSessionSummary(),
            callSummary: null);

        Assert.Contains("no-contact", signature, StringComparison.Ordinal);
        Assert.Contains("no-call", signature, StringComparison.Ordinal);
    }

    [Fact]
    public void Build_ChangesWhenRenderedHeaderInputsChange()
    {
        string original = ChatHeaderSignature.Build(
            CreateHeader(contactCard: CreateContactCard()),
            CreateSecuritySummary(),
            CreateSessionSummary(),
            CreateCallSummary());
        string changed = ChatHeaderSignature.Build(
            CreateHeader(contactCard: CreateContactCard(trustState: "identity_changed")),
            CreateSecuritySummary(),
            CreateSessionSummary(),
            CreateCallSummary(state: "ended"));

        Assert.NotEqual(original, changed);
    }

    private static ChatHeaderContactSummary CreateHeader(ContactCardSnapshot? contactCard)
        => new(
            DialogKey: "dialog:instance:peer",
            Title: "Peer",
            Subtitle: "Online",
            ContactCard: contactCard);

    private static ContactCardSnapshot CreateContactCard(string trustState = "verified")
        => new(
            Contact: new ContactSnapshot(
                ContactId: "contact-1",
                Petname: "Peer",
                RemoteAccountId: "account-peer",
                PublicDisplayName: "Peer",
                PublicAvatarHashHex: "avatar-hash",
                PublicAvatarMimeType: "image/png",
                PublicAvatarImagePath: null,
                PublicAvatarPendingHashHex: null,
                PublicAvatarPendingMimeType: null,
                PublicProfileUpdatedAtUnixMs: 101,
                RemoteInstanceId: "peer-instance",
                PrimaryDialogKey: "dialog:instance:peer",
                RelationshipState: "approved",
                PendingRequestDirection: null,
                HasMet: true,
                FriendRelayConsent: false,
                IsBlocked: false,
                PresenceState: "online",
                PresenceLastSeenAtUnixMs: 201,
                PresenceLastSentAtUnixMs: 202,
                PresenceLastActivityAtUnixMs: 203,
                RemovedAtUnixMs: null,
                CreatedAtUnixMs: 1,
                UpdatedAtUnixMs: 2),
            TrustState: trustState,
            Verification: new VerificationSummary(
                LastVerifiedAtUnixMs: 301,
                VerificationMethod: "manual_compare",
                CurrentIdentityKey: "identity-current",
                VerifiedIdentityKey: "identity-verified",
                DeviceCount: 2,
                CurrentFingerprint: "fingerprint-current",
                CurrentSafetyNumber: "safety-current",
                VerifiedFingerprint: "fingerprint-verified",
                VerifiedSafetyNumber: "safety-verified"));

    private static ChatSecuritySummary CreateSecuritySummary()
        => new(
            DialogKey: "dialog:instance:peer",
            ContactId: "contact-1",
            RemoteAccountId: "account-peer",
            RemoteDeviceId: "device-peer",
            TrustState: "verified",
            Warning: "none",
            HasEstablishedSession: true,
            HasRemotePrekeyBundle: true,
            UsesEncryptedTransport: true,
            SessionState: "established",
            LastSessionUpdateUnixMs: 401);

    private static ChatSessionSummary CreateSessionSummary()
        => new(
            IsOpenLease: true,
            AutoReconnectPaused: false,
            SessionStateValue: "connected",
            PeerEndpoint: "203.0.113.10:4433",
            LocalEndpoint: "192.0.2.10:4433",
            LastInteractionAtUnixMs: 501,
            LastInteractionKindValue: "message",
            LastConfirmedKeepaliveAtUnixMs: 502,
            LastRoundTripMs: 42);

    private static CallSummary CreateCallSummary(string state = "active")
        => new(
            CallId: "call-1",
            DialogKey: "dialog:instance:peer",
            ContactId: "contact-1",
            Direction: "outgoing",
            State: state,
            Outcome: null,
            DeclineReason: null,
            Participants:
            [
                new CallParticipantSummary(
                    ParticipantId: "participant-local",
                    AccountId: "account-local",
                    DeviceId: "device-local",
                    DisplayName: "Local",
                    IsLocal: true,
                    IsMuted: false),
                new CallParticipantSummary(
                    ParticipantId: "participant-remote",
                    AccountId: "account-peer",
                    DeviceId: "device-peer",
                    DisplayName: "Peer",
                    IsLocal: false,
                    IsMuted: true),
            ],
            LocalMicMuted: false,
            NetworkQuality: "good",
            StartedAtUnixMs: 601,
            RingingStartedAtUnixMs: 602,
            ConnectedAtUnixMs: 603,
            EndedAtUnixMs: null,
            ReconnectDeadlineUnixMs: null,
            UpdatedAtUnixMs: 604);
}
