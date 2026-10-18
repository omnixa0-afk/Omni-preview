using System;
using System.Collections.Generic;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Omni.Windows.App.Core;

// Remote screen control (docs/decisions/remote-screen-control.md). The core decides who may control
// and verifies every batch; the shell grants, captures the viewer's input and injects the sharer's.
internal sealed partial class RustBridge
{
    public void SetCallScreenControl(string callId, bool granted)
    {
        const string operation = "calls_set_screen_control";
        string payload = CallRaw(
            () => NativeMethods.omni_calls_set_screen_control(
                handle,
                JsonSerializer.Serialize(
                    new SetCallScreenControlRequest(callId, granted),
                    OmniJsonSerializerContext.Default.SetCallScreenControlRequest)),
            operation);
        ThrowIfRustError(payload, operation);
    }

    /// While we grant control: tell the controller what stands in the way ("locked", "owner_active")
    /// or that input lands again ("ready"). Does not change the grant.
    public void ReportCallScreenControlState(string callId, string sharerState)
    {
        const string operation = "calls_report_screen_control_state";
        string payload = CallRaw(
            () => NativeMethods.omni_calls_set_screen_control(
                handle,
                JsonSerializer.Serialize(
                    new SetCallScreenControlRequest(callId, true, sharerState),
                    OmniJsonSerializerContext.Default.SetCallScreenControlRequest)),
            operation);
        ThrowIfRustError(payload, operation);
    }

    public CallScreenControlState GetCallScreenControlState()
        => CallJson(
            () => NativeMethods.omni_calls_get_screen_control_state(handle),
            OmniJsonSerializerContext.Default.CallScreenControlState,
            "calls_get_screen_control_state");

    /// Nobody when `everyone` is false and `members` is empty.
    public void SetGroupCallScreenControl(string groupId, bool everyone, IReadOnlyList<string> members)
    {
        const string operation = "groups_set_call_screen_control";
        string payload = CallRaw(
            () => NativeMethods.omni_groups_set_call_screen_control(
                handle,
                JsonSerializer.Serialize(
                    new SetGroupCallScreenControlRequest(groupId, everyone, members),
                    OmniJsonSerializerContext.Default.SetGroupCallScreenControlRequest)),
            operation);
        ThrowIfRustError(payload, operation);
    }

    /// Our input to the screen we control; also the keep-alive (no events). `sharer` only in a group.
    public void SendScreenControl(string dialogKey, string? sharer, (ushort X, ushort Y)? pointer, IReadOnlyList<ScreenInputEventDto> events)
    {
        const string operation = "screen_control_send";
        string payload = CallRaw(
            () => NativeMethods.omni_screen_control_send(
                handle,
                JsonSerializer.Serialize(
                    new ScreenControlSendRequest(
                        dialogKey,
                        sharer,
                        pointer is { } p ? new[] { p.X, p.Y } : null,
                        events),
                    OmniJsonSerializerContext.Default.ScreenControlSendRequest)),
            operation);
        ThrowIfRustError(payload, operation);
    }

    /// Install the injector the core hands verified input to (`sink(context, json, len)`, called on
    /// the mesh pump thread). Zero removes it.
    public void SetScreenControlSink(IntPtr sink, IntPtr context)
    {
        lock (syncRoot)
        {
            EnsureInitialized();
            IntPtr error = NativeMethods.omni_screen_control_set_sink(handle, sink, context);
            if (error != IntPtr.Zero)
            {
                throw new InvalidOperationException(NativeErrorReader.ReadAndFree(error));
            }
        }
    }

    // --- request DTOs (match omni-ffi requests_calls.rs) ---
    internal sealed record SetCallScreenControlRequest(
        [property: JsonPropertyName("call_id")] string CallId,
        [property: JsonPropertyName("granted")] bool Granted,
        [property: JsonPropertyName("sharer_state"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] string? SharerState = null);

    internal sealed record SetGroupCallScreenControlRequest(
        [property: JsonPropertyName("group_id")] string GroupId,
        [property: JsonPropertyName("everyone")] bool Everyone,
        [property: JsonPropertyName("members")] IReadOnlyList<string> Members);

    internal sealed record ScreenControlSendRequest(
        [property: JsonPropertyName("dialog_key")] string DialogKey,
        [property: JsonPropertyName("sharer"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] string? Sharer,
        [property: JsonPropertyName("pointer"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] ushort[]? Pointer,
        [property: JsonPropertyName("events")] IReadOnlyList<ScreenInputEventDto> Events);
}

/// One input event, the core's `ScreenInputEvent`: `kind` is "button" (button, down), "wheel"
/// (dx, dy in WHEEL_DELTA units) or "key" (vk, scan, extended, down).
internal sealed record ScreenInputEventDto(
    [property: JsonPropertyName("kind")] string Kind,
    [property: JsonPropertyName("button"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] string? Button = null,
    [property: JsonPropertyName("down"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] bool? Down = null,
    [property: JsonPropertyName("dx"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] short? Dx = null,
    [property: JsonPropertyName("dy"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] short? Dy = null,
    [property: JsonPropertyName("vk"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] ushort? Vk = null,
    [property: JsonPropertyName("scan"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] ushort? Scan = null,
    [property: JsonPropertyName("extended"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] bool? Extended = null)
{
    public static ScreenInputEventDto ButtonEvent(string button, bool down) => new("button", Button: button, Down: down);

    public static ScreenInputEventDto WheelEvent(short dx, short dy) => new("wheel", Dx: dx, Dy: dy);

    public static ScreenInputEventDto KeyEvent(ushort vk, ushort scan, bool extended, bool down)
        => new("key", Down: down, Vk: vk, Scan: scan, Extended: extended);
}

/// A batch the core verified (omni-app `ScreenControlDelivery`).
internal sealed record ScreenControlDelivery(
    [property: JsonPropertyName("dialog_key")] string DialogKey,
    [property: JsonPropertyName("viewer")] string Viewer,
    [property: JsonPropertyName("pointer")] ushort[]? Pointer,
    [property: JsonPropertyName("events")] IReadOnlyList<ScreenInputEventDto> Events,
    [property: JsonPropertyName("release")] bool Release);

/// Screen control in the current 1:1 call.
internal sealed record CallScreenControlState(
    [property: JsonPropertyName("i_grant")] bool IGrant,
    [property: JsonPropertyName("peer_grants_me")] bool PeerGrantsMe,
    // What the sharer said about its side: "ready", "locked", "owner_active" (null: nothing said).
    [property: JsonPropertyName("peer_state")] string? PeerState = null);

/// Who may control a shared group screen (omni-protocol `ScreenControlGrantV1`).
internal sealed record ScreenControlGrant(
    [property: JsonPropertyName("everyone")] bool Everyone,
    [property: JsonPropertyName("members")] IReadOnlyList<string>? Members);
