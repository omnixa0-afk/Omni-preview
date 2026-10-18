using System.Collections.Generic;
using System.Text.Json.Serialization;

namespace Omni.Windows.App.Core;

// Find by name (docs/decisions/find-by-name.md): what the core found under a name.

/// <summary>
/// `found` · `nobody` (couriers answered, no one is findable by this name) · `unreachable` (no courier
/// answered — we do not know) · `too_short`.
/// </summary>
internal sealed record NameSearchResult(
    [property: JsonPropertyName("status")] string Status,
    [property: JsonPropertyName("people")] IReadOnlyList<FoundByName> People);

// Public relay directory (docs/decisions/public-relay-directory.md).

internal sealed record PublicRelayRole(
    [property: JsonPropertyName("on")] bool On,
    [property: JsonPropertyName("name")] string Name);

/// <summary>`found` · `nobody` (couriers answered, no relay) · `unreachable` (no courier answered).</summary>
internal sealed record RelaySearchResult(
    [property: JsonPropertyName("status")] string Status,
    [property: JsonPropertyName("answered")] IReadOnlyList<string> Answered,
    [property: JsonPropertyName("relays")] IReadOnlyList<PublicRelay> Relays);

internal sealed record PublicRelay(
    [property: JsonPropertyName("node_id")] string NodeId,
    [property: JsonPropertyName("name")] string Name,
    [property: JsonPropertyName("udp")] IReadOnlyList<string> Udp,
    [property: JsonPropertyName("tcp")] IReadOnlyList<string> Tcp,
    [property: JsonPropertyName("load")] string Load,
    [property: JsonPropertyName("seniority_secs")] long SenioritySecs,
    [property: JsonPropertyName("found_on")] IReadOnlyList<string> FoundOn,
    [property: JsonPropertyName("score")] double Score);

/// <summary>The relay this profile goes through («Сидеть через него», stage 3).</summary>
internal sealed record PinnedRelay(
    [property: JsonPropertyName("node_id")] string NodeId,
    [property: JsonPropertyName("name")] string Name,
    [property: JsonPropertyName("udp")] string Udp,
    [property: JsonPropertyName("tcp")] string? Tcp,
    [property: JsonPropertyName("always")] bool Always,
    [property: JsonPropertyName("keep_address_hidden")] bool KeepAddressHidden);

/// <summary>`LostSecs` — lost that long ago; null while held (or not overdue yet).</summary>
internal sealed record PinnedRelayState(
    [property: JsonPropertyName("relay")] PinnedRelay? Relay,
    [property: JsonPropertyName("linked")] bool Linked,
    [property: JsonPropertyName("lost_secs")] long? LostSecs);

internal sealed record FoundByName(
    [property: JsonPropertyName("omni_id")] string OmniId,
    [property: JsonPropertyName("display_name")] string DisplayName,
    [property: JsonPropertyName("photo_jpeg_b64")] string? PhotoJpegB64,
    [property: JsonPropertyName("is_contact")] bool IsContact,
    [property: JsonPropertyName("is_self")] bool IsSelf,
    [property: JsonPropertyName("whole_name")] bool WholeName);
