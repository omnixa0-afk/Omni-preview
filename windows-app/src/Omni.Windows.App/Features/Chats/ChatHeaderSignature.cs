using Omni.Windows.App.Core;

namespace Omni.Windows.App.Features.Chats;

internal static class ChatHeaderSignature
{
    internal static string Build(
        ChatHeaderContactSummary headerSummary,
        ChatSecuritySummary securitySummary,
        ChatSessionSummary sessionSummary,
        CallSummary? callSummary)
    {
        string contactSignature = headerSummary.ContactCard is null
            ? "no-contact"
            : string.Join(':',
                headerSummary.ContactCard.Contact.ContactId,
                headerSummary.ContactCard.Contact.Petname,
                headerSummary.ContactCard.Contact.PublicDisplayName,
                headerSummary.ContactCard.Contact.PublicAvatarHashHex,
                headerSummary.ContactCard.Contact.PublicAvatarPendingHashHex,
                headerSummary.ContactCard.Contact.PublicAvatarPendingMimeType,
                headerSummary.ContactCard.Contact.PublicProfileUpdatedAtUnixMs,
                headerSummary.ContactCard.Contact.RemoteAccountId,
                headerSummary.ContactCard.Contact.RelationshipState,
                headerSummary.ContactCard.Contact.IsBlocked,
                headerSummary.ContactCard.Contact.RemovedAtUnixMs,
                headerSummary.ContactCard.TrustState,
                headerSummary.ContactCard.Verification.LastVerifiedAtUnixMs,
                headerSummary.ContactCard.Verification.VerificationMethod,
                headerSummary.ContactCard.Verification.CurrentIdentityKey,
                headerSummary.ContactCard.Verification.VerifiedIdentityKey,
                headerSummary.ContactCard.Verification.DeviceCount,
                headerSummary.ContactCard.Verification.CurrentFingerprint,
                headerSummary.ContactCard.Verification.CurrentSafetyNumber,
                headerSummary.ContactCard.Verification.VerifiedFingerprint,
                headerSummary.ContactCard.Verification.VerifiedSafetyNumber);

        string callSignature = callSummary is null
            ? "no-call"
            : string.Join(':',
                callSummary.CallId,
                callSummary.Direction,
                callSummary.State,
                callSummary.Outcome,
                callSummary.DeclineReason,
                callSummary.LocalMicMuted,
                callSummary.NetworkQuality,
                callSummary.StartedAtUnixMs,
                callSummary.RingingStartedAtUnixMs,
                callSummary.ConnectedAtUnixMs,
                callSummary.EndedAtUnixMs,
                callSummary.ReconnectDeadlineUnixMs,
                callSummary.UpdatedAtUnixMs,
                string.Join(',', callSummary.Participants.Select(participant =>
                    $"{participant.ParticipantId}>{participant.AccountId}>{participant.DeviceId}>{participant.DisplayName}>{participant.IsLocal}>{participant.IsMuted}")));

        return string.Join('|',
            headerSummary.DialogKey,
            headerSummary.Title,
            headerSummary.Subtitle,
            contactSignature,
            securitySummary.DialogKey,
            securitySummary.ContactId,
            securitySummary.RemoteAccountId,
            securitySummary.RemoteDeviceId,
            securitySummary.TrustState,
            securitySummary.Warning,
            securitySummary.HasEstablishedSession,
            securitySummary.HasRemotePrekeyBundle,
            securitySummary.UsesEncryptedTransport,
            securitySummary.SessionState,
            securitySummary.LastSessionUpdateUnixMs,
            sessionSummary.IsOpenLease,
            sessionSummary.AutoReconnectPaused,
            sessionSummary.SessionStateValue,
            sessionSummary.PeerEndpoint,
            sessionSummary.LocalEndpoint,
            sessionSummary.LastInteractionAtUnixMs,
            sessionSummary.LastInteractionKindValue,
            sessionSummary.LastConfirmedKeepaliveAtUnixMs,
            sessionSummary.LastRoundTripMs,
            callSignature);
    }
}
