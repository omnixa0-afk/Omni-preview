using System.Collections.Generic;
using System.Text.Json.Serialization;

namespace Omni.Windows.App.Core;
internal sealed record TransportSnapshot(
    [property: JsonPropertyName("state")] string State,
    [property: JsonPropertyName("transport_mode")] string TransportModeValue,
    [property: JsonPropertyName("peer_id")] string? PeerId,
    [property: JsonPropertyName("peer_key")] string? PeerKey,
    [property: JsonPropertyName("remote_instance_id")] string? RemoteInstanceId,
    [property: JsonPropertyName("remote_account_id")] string? RemoteAccountId,
    [property: JsonPropertyName("remote_device_name")] string? RemoteDeviceName,
    [property: JsonPropertyName("initiated_locally")] bool InitiatedLocally,
    [property: JsonPropertyName("last_error")] string? LastError)
{
    [JsonPropertyName("duplicate_resolved")]
    public bool DuplicateResolved { get; init; }
}

internal sealed record TransportDiscoveryRefreshPayload(
    [property: JsonPropertyName("transport_snapshot")] TransportSnapshot TransportSnapshot,
    [property: JsonPropertyName("queue_diagnostics")] QueueDiagnosticsSnapshot QueueDiagnosticsSnapshot);

internal sealed record NetworkInterfaceSnapshot(
    [property: JsonPropertyName("id")] string Id,
    [property: JsonPropertyName("display_name")] string DisplayName,
    [property: JsonPropertyName("is_up")] bool IsUp,
    [property: JsonPropertyName("is_virtual")] bool IsVirtual,
    [property: JsonPropertyName("is_loopback")] bool IsLoopback,
    [property: JsonPropertyName("supports_broadcast")] bool SupportsBroadcast,
    [property: JsonPropertyName("supports_multicast")] bool SupportsMulticast,
    [property: JsonPropertyName("ipv4_addresses")] IReadOnlyList<string> IPv4Addresses,
    [property: JsonPropertyName("prefix_lengths")] IReadOnlyList<byte> PrefixLengths,
    [property: JsonPropertyName("ipv6_addresses")] IReadOnlyList<string>? IPv6Addresses = null);

internal sealed record OutboundQueueRecord(
    [property: JsonPropertyName("message_id")] string MessageId,
    [property: JsonPropertyName("peer_addr")] string DialogKey,
    [property: JsonPropertyName("status")] string Status,
    [property: JsonPropertyName("attempt_count")] long AttemptCount,
    [property: JsonPropertyName("next_attempt_unix_ms")] long? NextAttemptUnixMs,
    [property: JsonPropertyName("last_attempt_unix_ms")] long? LastAttemptUnixMs,
    [property: JsonPropertyName("last_failure_kind")] string? LastFailureKind,
    [property: JsonPropertyName("last_failure_message")] string? LastFailureMessage,
    [property: JsonPropertyName("remote_delivered_at_unix_ms")] long? RemoteDeliveredAtUnixMs,
    [property: JsonPropertyName("created_at_unix_ms")] long CreatedAtUnixMs,
    [property: JsonPropertyName("updated_at_unix_ms")] long UpdatedAtUnixMs);

internal sealed record OutboundQueueSnapshot(
    [property: JsonPropertyName("queued_count")] int QueuedCount,
    [property: JsonPropertyName("sending_count")] int SendingCount,
    [property: JsonPropertyName("retrying_count")] int RetryingCount,
    [property: JsonPropertyName("sent_waiting_ack_count")] int SentWaitingAckCount,
    [property: JsonPropertyName("failed_count")] int FailedCount,
    [property: JsonPropertyName("items")] IReadOnlyList<OutboundQueueRecord> Items);

internal sealed record DialogQueueDiagnostics(
    [property: JsonPropertyName("peer_addr")] string DialogKey,
    [property: JsonPropertyName("pending_count")] int PendingCount,
    [property: JsonPropertyName("sent_waiting_ack_count")] int SentWaitingAckCount,
    [property: JsonPropertyName("retrying_count")] int RetryingCount,
    [property: JsonPropertyName("failed_count")] int FailedCount,
    [property: JsonPropertyName("last_failure_kind")] string? LastFailureKind,
    [property: JsonPropertyName("last_failure_message")] string? LastFailureMessage);

internal sealed record QueueDiagnosticsSnapshot(
    [property: JsonPropertyName("queue_length")] int QueueLength,
    [property: JsonPropertyName("queued_count")] int QueuedCount,
    [property: JsonPropertyName("sending_count")] int SendingCount,
    [property: JsonPropertyName("retrying_count")] int RetryingCount,
    [property: JsonPropertyName("sent_waiting_ack_count")] int SentWaitingAckCount,
    [property: JsonPropertyName("failed_count")] int FailedCount,
    [property: JsonPropertyName("last_delivery_error")] string? LastDeliveryError,
    [property: JsonPropertyName("last_failure_kind")] string? LastFailureKind,
    [property: JsonPropertyName("last_failure_message")] string? LastFailureMessage,
    [property: JsonPropertyName("pending_by_dialog")] IReadOnlyList<DialogQueueDiagnostics> PendingByDialog);

internal sealed record EntityRevisionSnapshot(
    [property: JsonPropertyName("entity_id")] string EntityId,
    [property: JsonPropertyName("revision")] long Revision);

internal sealed partial class RustBridge
{
}
