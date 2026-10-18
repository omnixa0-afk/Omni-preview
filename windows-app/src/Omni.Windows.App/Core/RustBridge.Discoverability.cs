using System.Text.Json;
using System.Text.Json.Serialization;

namespace Omni.Windows.App.Core;

internal sealed partial class RustBridge
{
    /// My shareable Omni-ID (`omni:<40 hex>`) — hand it to someone so they can find
    /// me (when discoverability is Public) and send a contact request.
    public string GetMyOmniId()
        => CallJson(
            () => NativeMethods.omni_discoverability_get_omni_id(handle),
            OmniJsonSerializerContext.Default.String,
            "discoverability_get_omni_id");

    /// "off" | "public".
    public string GetDiscoverabilityMode()
        => CallJson(
            () => NativeMethods.omni_discoverability_get_mode(handle),
            OmniJsonSerializerContext.Default.String,
            "discoverability_get_mode");

    public void SetDiscoverabilityMode(string mode)
    {
        const string operation = "discoverability_set_mode";
        string payload = CallRaw(
            () => NativeMethods.omni_discoverability_set_mode(
                handle,
                JsonSerializer.Serialize(
                    new SetDiscoverabilityRequest(mode),
                    OmniJsonSerializerContext.Default.SetDiscoverabilityRequest)),
            operation);
        ThrowIfRustError(payload, operation);
    }

    /// Find a peer by their Omni-ID and send a contact request (Model A). Returns the
    /// new pending ContactSnapshot; the peer must accept. Throws if no beacon is found
    /// (peer not Public / offline) or the ID is invalid.
    public ContactSnapshot FindContactByOmniId(string omniId, string petname)
        => CallJson(
            () => NativeMethods.omni_discoverability_find_by_omni_id(
                handle,
                JsonSerializer.Serialize(
                    new FindByOmniIdRequest(omniId, petname),
                    OmniJsonSerializerContext.Default.FindByOmniIdRequest)),
            OmniJsonSerializerContext.Default.ContactSnapshot,
            "discoverability_find_by_omni_id");

    /// "Находить меня по имени" as set (it works only while findable by Omni-ID).
    public bool GetNameFindable()
        => CallJson(
            () => NativeMethods.omni_discoverability_get_name_findable(handle),
            OmniJsonSerializerContext.Default.Boolean,
            "discoverability_get_name_findable");

    public void SetNameFindable(bool on)
    {
        const string operation = "discoverability_set_name_findable";
        string payload = CallRaw(
            () => NativeMethods.omni_discoverability_set_name_findable(
                handle,
                JsonSerializer.Serialize(new SetNameFindableRequest(on), OmniJsonSerializerContext.Default.SetNameFindableRequest)),
            operation);
        ThrowIfRustError(payload, operation);
    }

    /// People who published this name (docs/decisions/find-by-name.md): verified, one row per account.
    public NameSearchResult SearchPeopleByName(string query)
        => CallJson(
            () => NativeMethods.omni_discoverability_search_by_name(
                handle,
                JsonSerializer.Serialize(new SearchByNameRequest(query), OmniJsonSerializerContext.Default.SearchByNameRequest)),
            OmniJsonSerializerContext.Default.NameSearchResult,
            "discoverability_search_by_name");

    /// «Быть релеем для всех» (docs/decisions/public-relay-directory.md): the switch and the name.
    public PublicRelayRole GetPublicRelayRole()
        => CallJson(
            () => NativeMethods.omni_relay_directory_get_role(handle),
            OmniJsonSerializerContext.Default.PublicRelayRole,
            "relay_directory_get_role");

    public void SetPublicRelayRole(bool on, string name)
    {
        const string operation = "relay_directory_set_role";
        string payload = CallRaw(
            () => NativeMethods.omni_relay_directory_set_role(
                handle,
                JsonSerializer.Serialize(new PublicRelayRole(on, name), OmniJsonSerializerContext.Default.PublicRelayRole)),
            operation);
        ThrowIfRustError(payload, operation);
    }

    /// «Сидеть через него» (public-relay-directory.md, stage 3): the pin, set (replaces) and clear.
    public PinnedRelayState GetPinnedRelay()
        => CallJson(
            () => NativeMethods.omni_pinned_relay_get(handle),
            OmniJsonSerializerContext.Default.PinnedRelayState,
            "pinned_relay_get");

    public void PinRelay(PinnedRelay relay)
    {
        const string operation = "pinned_relay_set";
        string payload = CallRaw(
            () => NativeMethods.omni_pinned_relay_set(
                handle,
                JsonSerializer.Serialize(relay, OmniJsonSerializerContext.Default.PinnedRelay)),
            operation);
        ThrowIfRustError(payload, operation);
    }

    public void UnpinRelay()
    {
        const string operation = "pinned_relay_clear";
        string payload = CallRaw(() => NativeMethods.omni_pinned_relay_clear(handle), operation);
        ThrowIfRustError(payload, operation);
    }

    /// Public relays in the directory: verified, freshest per relay, ranked. Network — call off the UI thread.
    public RelaySearchResult SearchPublicRelays()
        => CallJson(
            () => NativeMethods.omni_relay_directory_search(handle),
            OmniJsonSerializerContext.Default.RelaySearchResult,
            "relay_directory_search");

    internal sealed record SetNameFindableRequest(
        [property: JsonPropertyName("on")] bool On);

    internal sealed record SearchByNameRequest(
        [property: JsonPropertyName("query")] string Query);

    internal sealed record SetDiscoverabilityRequest(
        [property: JsonPropertyName("mode")] string Mode);

    internal sealed record FindByOmniIdRequest(
        [property: JsonPropertyName("omni_id")] string OmniId,
        [property: JsonPropertyName("petname")] string Petname);
}
