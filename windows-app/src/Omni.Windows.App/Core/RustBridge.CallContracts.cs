using System.Collections.Generic;
using System.Text.Json.Serialization;

namespace Omni.Windows.App.Core;

internal sealed record CallParticipantSummary(
    [property: JsonPropertyName("participant_id")] string ParticipantId,
    [property: JsonPropertyName("account_id")] string AccountId,
    [property: JsonPropertyName("device_id")] string DeviceId,
    [property: JsonPropertyName("display_name")] string? DisplayName,
    [property: JsonPropertyName("is_local")] bool IsLocal,
    [property: JsonPropertyName("is_muted")] bool IsMuted);

internal sealed record CallHistoryEvent(
    [property: JsonPropertyName("call_id")] string CallId,
    [property: JsonPropertyName("direction")] string Direction,
    [property: JsonPropertyName("outcome")] string Outcome,
    [property: JsonPropertyName("decline_reason")] string? DeclineReason,
    [property: JsonPropertyName("participants")] IReadOnlyList<CallParticipantSummary> Participants,
    [property: JsonPropertyName("started_at_unix_ms")] long StartedAtUnixMs,
    [property: JsonPropertyName("connected_at_unix_ms")] long? ConnectedAtUnixMs,
    [property: JsonPropertyName("ended_at_unix_ms")] long? EndedAtUnixMs,
    [property: JsonPropertyName("duration_seconds")] uint? DurationSeconds,
    [property: JsonPropertyName("remote_control")] bool RemoteControl = false);

/// The remote-control side of a call (docs/decisions/remote-access-for-contacts.md). Role:
/// "controller" (we control the peer) or "owner" (the peer controls us). Phase: "awaiting_owner",
/// "approved", "refused".
internal sealed record RemoteControlSession(
    [property: JsonPropertyName("role")] string Role,
    [property: JsonPropertyName("phase")] string Phase,
    [property: JsonPropertyName("answer_deadline_unix_ms")] long? AnswerDeadlineUnixMs)
{
    public bool IsOwner => Role == "owner";
    public bool IsController => Role == "controller";
    public bool IsApproved => Phase == "approved";
    public bool IsAwaitingOwner => Phase == "awaiting_owner";
}

internal sealed record CallSummary(
    [property: JsonPropertyName("call_id")] string CallId,
    [property: JsonPropertyName("dialog_key")] string DialogKey,
    [property: JsonPropertyName("contact_id")] string? ContactId,
    [property: JsonPropertyName("direction")] string Direction,
    [property: JsonPropertyName("state")] string State,
    [property: JsonPropertyName("outcome")] string? Outcome,
    [property: JsonPropertyName("decline_reason")] string? DeclineReason,
    [property: JsonPropertyName("participants")] IReadOnlyList<CallParticipantSummary> Participants,
    [property: JsonPropertyName("local_mic_muted")] bool LocalMicMuted,
    [property: JsonPropertyName("network_quality")] string? NetworkQuality,
    [property: JsonPropertyName("started_at_unix_ms")] long StartedAtUnixMs,
    [property: JsonPropertyName("ringing_started_at_unix_ms")] long? RingingStartedAtUnixMs,
    [property: JsonPropertyName("connected_at_unix_ms")] long? ConnectedAtUnixMs,
    [property: JsonPropertyName("ended_at_unix_ms")] long? EndedAtUnixMs,
    [property: JsonPropertyName("reconnect_deadline_unix_ms")] long? ReconnectDeadlineUnixMs,
    [property: JsonPropertyName("updated_at_unix_ms")] long UpdatedAtUnixMs,
    [property: JsonPropertyName("remote_control")] RemoteControlSession? RemoteControl = null);

internal sealed record CallInboxSnapshot(
    [property: JsonPropertyName("current_call")] CallSummary? CurrentCall,
    [property: JsonPropertyName("incoming_calls")] IReadOnlyList<CallSummary> IncomingCalls,
    [property: JsonPropertyName("updated_at_unix_ms")] long UpdatedAtUnixMs);

/// <summary>One half of what this machine does with a codec (encode or decode), as probed.</summary>
internal sealed record RealtimeVideoCodecSide(
    [property: JsonPropertyName("hardware")] bool Hardware,
    [property: JsonPropertyName("max_width")] uint MaxWidth = 0,
    [property: JsonPropertyName("max_height")] uint MaxHeight = 0,
    [property: JsonPropertyName("max_fps")] uint MaxFps = 0);

/// <summary>
/// A codec line of the capability passport: encodes it? decodes it? in hardware? The core picks the
/// codec per direction from both passports (docs/decisions/per-pair-video-codec-selection.md).
/// </summary>
internal sealed record RealtimeVideoCodecCapability(
    [property: JsonPropertyName("codec")] string Codec,
    [property: JsonPropertyName("payload_type")] byte PayloadType,
    [property: JsonPropertyName("encode")] RealtimeVideoCodecSide? Encode,
    [property: JsonPropertyName("decode")] RealtimeVideoCodecSide? Decode,
    [property: JsonPropertyName("implementation")] string? Implementation);

/// <summary>
/// The video format range this peer supports — NOT just what its camera does. Both
/// peers constrain it because whoever receives has to decode it, so a machine with
/// no camera still has a say; <c>CanSendVideo</c> is the part that is about the
/// camera. See the Rust <c>RealtimeVideoProfileCapabilityV1</c>.
/// </summary>
internal sealed record RealtimeVideoProfileCapability(
    [property: JsonPropertyName("min_width")] uint MinWidth,
    [property: JsonPropertyName("min_height")] uint MinHeight,
    [property: JsonPropertyName("min_fps")] uint MinFps,
    [property: JsonPropertyName("preferred_width")] uint PreferredWidth,
    [property: JsonPropertyName("preferred_height")] uint PreferredHeight,
    [property: JsonPropertyName("preferred_fps")] uint PreferredFps,
    [property: JsonPropertyName("max_width")] uint MaxWidth,
    [property: JsonPropertyName("max_height")] uint MaxHeight,
    [property: JsonPropertyName("max_fps")] uint MaxFps,
    [property: JsonPropertyName("pixel_formats")] IReadOnlyList<string> PixelFormats,
    [property: JsonPropertyName("can_send_video")] bool CanSendVideo);

/// <summary>
/// What this peer brings to a call. Audio needs no negotiation, so every field here
/// is about video and every one may say "none": empty <c>Codecs</c> and/or a null
/// <c>Video</c> is how a machine without a camera or a hardware encoder announces
/// itself — and it MUST still announce itself, because this is also what brings the
/// call's audio up on both sides.
/// </summary>
internal sealed record RealtimeMediaCapabilities(
    [property: JsonPropertyName("dialog_key")] string DialogKey,
    [property: JsonPropertyName("codecs")] IReadOnlyList<RealtimeVideoCodecCapability> Codecs,
    [property: JsonPropertyName("video")] RealtimeVideoProfileCapability? Video,
    [property: JsonPropertyName("max_datagram_bytes")] uint MaxDatagramBytes,
    [property: JsonPropertyName("supports_rtcp_feedback")] bool SupportsRtcpFeedback,
    [property: JsonPropertyName("sent_at_unix_ms")] long SentAtUnixMs,
    // "mains" | "battery": software decoding on a battery is the expensive kind.
    [property: JsonPropertyName("power")] string Power = "mains",
    // "low" | "medium" | "high": software encoding only when the processor is idle enough.
    [property: JsonPropertyName("cpu_load")] string CpuLoad = "medium",
    // The user's ceiling (Settings → Calls → Кодек), null = Auto.
    [property: JsonPropertyName("codec_ceiling")] string? CodecCeiling = null,
    // Settings → Calls → Битрейт: "economy" | "auto" | "max", scales both directions' ceiling.
    [property: JsonPropertyName("bitrate_level")] string BitrateLevel = "auto",
    // "camera" | "screen": what this machine sends now; its direction's codec is chosen for it.
    [property: JsonPropertyName("content")] string Content = "camera");

/// <summary>The codec of one direction of a call, labelled by its sender's announcement.</summary>
/// <summary>One direction of a call: codec and format of its own (docs/decisions/per-direction-video-format.md).</summary>
internal sealed record RealtimeVideoStream(
    [property: JsonPropertyName("sender_capabilities_id")] string SenderCapabilitiesId,
    [property: JsonPropertyName("codec")] string Codec,
    [property: JsonPropertyName("payload_type")] byte PayloadType,
    [property: JsonPropertyName("width")] uint Width,
    [property: JsonPropertyName("height")] uint Height,
    [property: JsonPropertyName("fps")] uint Fps,
    [property: JsonPropertyName("target_bitrate_bps")] uint TargetBitrateBps,
    [property: JsonPropertyName("reason")] string? Reason);

internal sealed record RealtimeVideoSelection(
    [property: JsonPropertyName("streams")] IReadOnlyList<RealtimeVideoStream> Streams,
    [property: JsonPropertyName("capture_pixel_format")] string CapturePixelFormat,
    [property: JsonPropertyName("rtcp_feedback_enabled")] bool RtcpFeedbackEnabled);

/// <summary>
/// The agreed media session. A null <c>Video</c> is an ordinary working call — audio
/// both ways, no video — not a failure; the snapshot's Diagnostic says why video is
/// off. The selection existing at all is the signal to bring the audio pipeline up.
/// </summary>
internal sealed record RealtimeMediaSelection(
    [property: JsonPropertyName("dialog_key")] string DialogKey,
    [property: JsonPropertyName("session_id")] string SessionId,
    [property: JsonPropertyName("max_datagram_bytes")] uint MaxDatagramBytes,
    [property: JsonPropertyName("video")] RealtimeVideoSelection? Video,
    [property: JsonPropertyName("selected_at_unix_ms")] long SelectedAtUnixMs)
{
    /// <summary>What THIS side sends (filled from the snapshot, <see cref="RealtimeMediaNegotiationSnapshot.LocalSelection"/>).</summary>
    [JsonIgnore]
    internal RealtimeVideoStream? SendStream { get; init; }

    /// <summary>What THIS side receives.</summary>
    [JsonIgnore]
    internal RealtimeVideoStream? ReceiveStream { get; init; }
}

internal sealed record RealtimeMediaNegotiationSnapshot(
    [property: JsonPropertyName("dialog_key")] string DialogKey,
    [property: JsonPropertyName("status")] string Status,
    [property: JsonPropertyName("local_capabilities")] RealtimeMediaCapabilities? LocalCapabilities,
    [property: JsonPropertyName("remote_capabilities")] RealtimeMediaCapabilities? RemoteCapabilities,
    [property: JsonPropertyName("selected")] RealtimeMediaSelection? Selected,
    [property: JsonPropertyName("diagnostic")] string? Diagnostic,
    [property: JsonPropertyName("updated_at_unix_ms")] long UpdatedAtUnixMs,
    [property: JsonPropertyName("send_video")] RealtimeVideoStream? SendVideo = null,
    [property: JsonPropertyName("receive_video")] RealtimeVideoStream? ReceiveVideo = null)
{
    /// <summary>The selection with this side's two directions attached — what the media service runs.</summary>
    internal RealtimeMediaSelection? LocalSelection =>
        Selected is null ? null : Selected with { SendStream = SendVideo, ReceiveStream = ReceiveVideo };
}

internal sealed partial class RustBridge
{
    internal sealed record CallIdRequest([property: JsonPropertyName("call_id")] string CallId);

    internal sealed record DeclineCallRequest(
        [property: JsonPropertyName("call_id")] string CallId,
        [property: JsonPropertyName("reason")] string Reason);

    internal sealed record SetLocalMicMutedRequest(
        [property: JsonPropertyName("call_id")] string CallId,
        [property: JsonPropertyName("is_muted")] bool IsMuted);

    internal sealed record PublishRealtimeMediaCapabilitiesRequest(
        [property: JsonPropertyName("capabilities")] RealtimeMediaCapabilities Capabilities);

    internal sealed record RealtimeMediaDialogRequest(
        [property: JsonPropertyName("dialog_key")] string DialogKey);
}
