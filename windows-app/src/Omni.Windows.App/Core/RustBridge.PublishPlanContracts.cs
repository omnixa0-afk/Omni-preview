using System.Collections.Generic;
using System.Text.Json.Serialization;

namespace Omni.Windows.App.Core;

// Where and how often each publication goes (docs/decisions/public-relay-directory.md §10).

internal sealed record PublishBlock(
    [property: JsonPropertyName("kind")] string Kind,
    [property: JsonPropertyName("every_secs")] int EverySecs);

/// <summary>Steps top to bottom; the blocks of a step at once, the next only if none took it.</summary>
internal sealed record PublishPlan(
    [property: JsonPropertyName("auto")] bool Auto,
    [property: JsonPropertyName("steps")] IReadOnlyList<IReadOnlyList<PublishBlock>> Steps);

/// <summary>A kind the publication cannot use, and why (shown switched off, not hidden).</summary>
internal sealed record UnavailableKind(
    [property: JsonPropertyName("kind")] string Kind,
    [property: JsonPropertyName("reason")] string Reason);

/// <summary>`contacts` · `omni_id` · `name` · `relay`, its plan, the kinds it may use, one line.</summary>
internal sealed record PublishPlanView(
    [property: JsonPropertyName("publication")] string Publication,
    [property: JsonPropertyName("plan")] PublishPlan Plan,
    [property: JsonPropertyName("allowed_kinds")] IReadOnlyList<string> AllowedKinds,
    [property: JsonPropertyName("unavailable")] IReadOnlyList<UnavailableKind> Unavailable,
    [property: JsonPropertyName("summary")] string Summary);
