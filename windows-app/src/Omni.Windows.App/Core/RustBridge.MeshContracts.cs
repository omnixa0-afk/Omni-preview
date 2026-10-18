using System.Collections.Generic;
using System.Text.Json.Serialization;

namespace Omni.Windows.App.Core;

// DTOs for the honest connection-state surface (omni_mesh_connection_state). Flat records with
// string `kind`s + optional fields — the Rust core derives the state (Rust = truth), the shell
// only renders. See docs/decisions/omni-network-model.md.

/// <summary>This node's connectivity + any directly-dialable public endpoints.
/// <c>Kind</c> ∈ { not_started, isolated, connecting, online_behind_nat, online_reachable }.</summary>
internal sealed record NodeConnectivityDto(
    [property: JsonPropertyName("kind")] string Kind,
    [property: JsonPropertyName("endpoints")] IReadOnlyList<string>? Endpoints);

/// <summary>Per-contact connection state. <c>Link</c> ∈ { online_direct, online_via_relay,
/// searching, will_deliver_later, cannot_reach }. <c>RelayHops</c> set only when relayed;
/// <c>CannotReachReason</c> set only when cannot_reach ∈ { peer_offline, no_route_both_unreachable,
/// rendezvous_disabled, address_stale, not_approved, verification_failed }. <c>Verified</c>
/// (has_met) is orthogonal to the link.</summary>
internal sealed record ContactConnectionDto(
    [property: JsonPropertyName("account_id")] string AccountId,
    [property: JsonPropertyName("link")] string Link,
    [property: JsonPropertyName("relay_hops")] int? RelayHops,
    // The transport carrier of the current best link: "quic" / "tcp_door" / "tcp_fallback" / "lan".
    // Present only while linked — lets the header say "напрямую · QUIC" vs "· TCP-фолбэк" instead of
    // a bare "online". "tcp_door" is still a DIRECT path (the peer's own TCP door, no middleman), it
    // just rides TCP instead of UDP — before it existed that case arrived as "quic" and the header
    // wrote "· QUIC" over a TCP connection.
    [property: JsonPropertyName("carrier")] string? Carrier,
    [property: JsonPropertyName("cannot_reach_reason")] string? CannotReachReason,
    [property: JsonPropertyName("verified")] bool Verified,
    [property: JsonPropertyName("trace")] IReadOnlyList<ConnectionStepDto>? Trace,
    // A device of the contact told us its couriers and none is common one way or the other: it can be
    // met only over a link that is already up. CouriersToEnable: ours, off or only read, that would help.
    [property: JsonPropertyName("no_common_courier")] bool NoCommonCourier,
    [property: JsonPropertyName("couriers_to_enable")] IReadOnlyList<string>? CouriersToEnable);

/// <summary>One step in a contact's live connection journey (Tier-2 trace). <c>Stage</c> ∈
/// { lan, rendezvous, linked }; <c>Status</c> ∈ { started, ok, failed, skipped }. <c>Detail</c> is
/// an optional human note; <c>AtMs</c> is the unix-ms when the step was last (re)recorded.</summary>
internal sealed record ConnectionStepDto(
    [property: JsonPropertyName("stage")] string Stage,
    [property: JsonPropertyName("status")] string Status,
    [property: JsonPropertyName("detail")] string? Detail,
    [property: JsonPropertyName("at_ms")] long AtMs);

/// <summary>The aggregate honest connection-state DTO: our node + per-contact links.</summary>
internal sealed record MeshConnectionStateDto(
    [property: JsonPropertyName("node")] NodeConnectivityDto Node,
    [property: JsonPropertyName("contacts")] IReadOnlyList<ContactConnectionDto> Contacts);

/// <summary>One network entry node (bootstrap / relay) for the «Узлы сети / Релеи» screen.
/// <c>Endpoint</c> = ip:port to dial; <c>NodeAddressHex</c> = mesh identity if known; <c>Source</c> ∈
/// { base, manual } — base is the one seeded, still-removable base node. <c>Removable</c> is always
/// true (even the base node can be deleted; the empty list still connects via LAN + invite).</summary>
internal sealed record NetworkNodeDto(
    [property: JsonPropertyName("id")] long Id,
    [property: JsonPropertyName("endpoint")] string? Endpoint,
    [property: JsonPropertyName("node_address_hex")] string? NodeAddressHex,
    [property: JsonPropertyName("label")] string Label,
    [property: JsonPropertyName("source")] string Source,
    [property: JsonPropertyName("removable")] bool Removable,
    [property: JsonPropertyName("status")] string Status,
    // R1c inc.3b: flagged as part of MY OWN relay bundle (auto-propagated to approved friends).
    [property: JsonPropertyName("shared")] bool Shared,
    // R1c inc.4: the friend's bundle this relay was imported under (+ its version), so the relay list
    // can group + label rows by set. Null for a manual / base / my-own row.
    [property: JsonPropertyName("bundle_id")] string? BundleId,
    [property: JsonPropertyName("bundle_version")] long? BundleVersion);

/// <summary>One rendezvous courier provider for the «Провайдеры рандеву» screen — a public pub/sub broker
/// (<c>http(s)://</c> ntfy / <c>ws(s)://</c> Nostr relay) the app fans beacons across to MEET a peer
/// (distinct from a relay, which carries transport). <c>Source</c> ∈ { base, manual, invite }: base is a
/// seeded default, invite was contributed by a friend's pairing invite. <c>Removable</c> is always true
/// (even a default can be deleted; the empty set falls back to the compiled default).</summary>
internal sealed record RendezvousProviderDto(
    [property: JsonPropertyName("id")] long Id,
    [property: JsonPropertyName("url")] string Url,
    [property: JsonPropertyName("source")] string Source,
    [property: JsonPropertyName("removable")] bool Removable,
    [property: JsonPropertyName("mode")] string Mode,
    [property: JsonPropertyName("kind")] string Kind,
    [property: JsonPropertyName("pooled")] bool Pooled,
    [property: JsonPropertyName("status")] string Status,
    [property: JsonPropertyName("status_detail")] string StatusDetail,
    [property: JsonPropertyName("status_age_secs")] long? StatusAgeSecs);
