using System.Buffers;
using System.Buffers.Binary;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using Omni.Windows.App.Features.Calls;

namespace Omni.Windows.App.Core;

internal sealed partial class RustBridge
{
    public CallSummary DeclineCall(string callId, string reason)
        => CallJson(
            () => NativeMethods.omni_calls_decline_call(
                handle,
                JsonSerializer.Serialize(
                    new DeclineCallRequest(callId, reason),
                    OmniJsonSerializerContext.Default.DeclineCallRequest)),
            OmniJsonSerializerContext.Default.CallSummary,
            "calls_decline_call");

    public CallSummary HangUpCall(string callId)
        => CallJson(
            () => NativeMethods.omni_calls_hang_up_call(
                handle,
                JsonSerializer.Serialize(
                    new CallIdRequest(callId),
                    OmniJsonSerializerContext.Default.CallIdRequest)),
            OmniJsonSerializerContext.Default.CallSummary,
            "calls_hang_up_call");

    public CallSummary PlaceCall(string dialogKey)
        => CallJson(
            () => NativeMethods.omni_calls_place_call(
                handle,
                JsonSerializer.Serialize(
                    new RealtimeMediaDialogRequest(dialogKey),
                    OmniJsonSerializerContext.Default.RealtimeMediaDialogRequest)),
            OmniJsonSerializerContext.Default.CallSummary,
            "calls_place_call");

    /// Start controlling a contact's computer (standing remote access) — a remote-control call.
    public CallSummary RequestRemoteControl(string contactId)
        => CallJson(
            () => NativeMethods.omni_calls_request_remote_control(
                handle,
                JsonSerializer.Serialize(
                    new ContactIdRequest(contactId),
                    OmniJsonSerializerContext.Default.ContactIdRequest)),
            OmniJsonSerializerContext.Default.CallSummary,
            "calls_request_remote_control");

    public CallSummary AcceptCall(string callId)
        => CallJson(
            () => NativeMethods.omni_calls_accept_call(
                handle,
                JsonSerializer.Serialize(
                    new CallIdRequest(callId),
                    OmniJsonSerializerContext.Default.CallIdRequest)),
            OmniJsonSerializerContext.Default.CallSummary,
            "calls_accept_call");

    public CallSummary SetLocalMicMuted(string callId, bool isMuted)
        => CallJson(
            () => NativeMethods.omni_calls_set_local_mic_muted(
                handle,
                JsonSerializer.Serialize(
                    new SetLocalMicMutedRequest(callId, isMuted),
                    OmniJsonSerializerContext.Default.SetLocalMicMutedRequest)),
            OmniJsonSerializerContext.Default.CallSummary,
            "calls_set_local_mic_muted");

    public CallInboxSnapshot GetCallInboxSnapshot()
        => CallJson(
            () => NativeMethods.omni_calls_get_inbox_snapshot(handle),
            OmniJsonSerializerContext.Default.CallInboxSnapshot,
            "calls_get_inbox_snapshot");

    public RealtimeMediaNegotiationSnapshot PublishRealtimeMediaCapabilities(RealtimeMediaCapabilities capabilities)
        => CallJson(
            () => NativeMethods.omni_calls_publish_realtime_media_capabilities(
                handle,
                JsonSerializer.Serialize(
                    new PublishRealtimeMediaCapabilitiesRequest(capabilities),
                    OmniJsonSerializerContext.Default.PublishRealtimeMediaCapabilitiesRequest)),
            OmniJsonSerializerContext.Default.RealtimeMediaNegotiationSnapshot,
            "calls_publish_realtime_media_capabilities");

    public RealtimeMediaNegotiationSnapshot GetRealtimeMediaNegotiation(string dialogKey)
        => CallJson(
            () => NativeMethods.omni_calls_get_realtime_media_negotiation(
                handle,
                JsonSerializer.Serialize(
                    new RealtimeMediaDialogRequest(dialogKey),
                    OmniJsonSerializerContext.Default.RealtimeMediaDialogRequest)),
            OmniJsonSerializerContext.Default.RealtimeMediaNegotiationSnapshot,
            "calls_get_realtime_media_negotiation");

    public void ClearRealtimeMediaNegotiation(string dialogKey)
    {
        const string operation = "calls_clear_realtime_media_negotiation";
        string payload = CallRaw(
            () => NativeMethods.omni_calls_clear_realtime_media_negotiation(
                handle,
                JsonSerializer.Serialize(
                    new RealtimeMediaDialogRequest(dialogKey),
                    OmniJsonSerializerContext.Default.RealtimeMediaDialogRequest)),
            operation);
        ThrowIfRustError(payload, operation);
    }
}
