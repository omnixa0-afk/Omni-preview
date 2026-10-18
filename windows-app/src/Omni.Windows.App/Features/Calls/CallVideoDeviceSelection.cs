namespace Omni.Windows.App.Features.Calls;

/// <summary>Which half of the H.264 pipeline a device choice applies to.</summary>
internal enum CallVideoMediaDirection
{
    Encode,
    Decode,
}

/// <summary>
/// One selectable entry in a Settings encode/decode device dropdown: a stable
/// <see cref="Key"/> (persisted) plus a human <see cref="DisplayName"/>.
/// </summary>
internal sealed record CallVideoDeviceOption(string Key, string DisplayName);

/// <summary>
/// The concrete decision the media pipeline acts on after a user preference is
/// resolved against the live capability probe. Either software, or a specific
/// GPU identified by PCI <see cref="VendorId"/> (which the native encoder MFT
/// filter / renderer adapter choice consume).
/// </summary>
internal readonly record struct ResolvedVideoDevice(bool UseSoftware, uint VendorId, string Label)
{
    internal static ResolvedVideoDevice Software(string label) => new(true, 0, label);

    internal static ResolvedVideoDevice Hardware(uint vendorId, string label) => new(false, vendorId, label);
}

/// <summary>
/// Maps a user's per-direction device preference (Auto / Software / a specific
/// GPU description) onto the real capabilities reported by the native probe.
///
/// The dropdown only ever offers hardware that genuinely works in that direction
/// (decode = real D3D11 profile check, encode = functional test-encode), so e.g.
/// Intel QSV — which configures but fails to actually encode — is never listed as
/// an encoder. When a direction has no capable hardware the list collapses to
/// Auto + Software (Auto then resolves to software), which is the honest empty
/// state the product wants.
///
/// "Good auto-select" (the default) is energy-first: prefer an integrated GPU's
/// media engine when it can do the job (the iGPU draws far less power than waking
/// the discrete GPU for a 1:1 720p call), else any working hardware, else software.
/// </summary>
internal static class CallVideoDeviceSelection
{
    internal const string AutoKey = "auto";
    internal const string SoftwareKey = "software";

    internal static bool IsCapable(NativeVideoDevice device, CallVideoMediaDirection direction)
        => direction == CallVideoMediaDirection.Encode ? device.CanEncodeH264 : device.CanDecodeH264;

    // Whether a "Software" choice is offered for a direction. Decode has a real software path
    // (libwebrtc OpenH264 — already the HW-decode fallback), so it offers Software. For ENCODE we
    // DELIBERATELY do not offer Software (decision 2026-06-07): HW H.264 encode is effectively
    // ubiquitous on target devices, and a CPU encoder is a battery/quality regression we don't
    // want as a user-pickable option. The session stays HW-encode-only by design — this is a
    // conscious choice, not a TODO. (If a HW-encoder-less device ever matters, revisit here.)
    internal static bool SoftwareSelectable(CallVideoMediaDirection direction)
        => direction == CallVideoMediaDirection.Decode;

    /// <summary>
    /// Build the dropdown options for one direction: Auto, Software, then every GPU
    /// that can actually do that direction. No capable hardware -> just Auto + Software.
    /// </summary>
    internal static IReadOnlyList<CallVideoDeviceOption> BuildOptions(
        CallVideoMediaDirection direction,
        IReadOnlyList<NativeVideoDevice> devices)
    {
        List<CallVideoDeviceOption> options = [new(AutoKey, "Авто")];
        if (SoftwareSelectable(direction))
        {
            options.Add(new CallVideoDeviceOption(SoftwareKey, "Программно (CPU)"));
        }

        foreach (NativeVideoDevice device in devices)
        {
            if (!IsCapable(device, direction))
            {
                continue;
            }

            string suffix = device.Integrated ? " · встроенная" : " · дискретная";
            options.Add(new CallVideoDeviceOption(device.Description, device.Description + suffix));
        }

        return options;
    }

    /// <summary>
    /// Resolve a stored preference key against the live device list into a concrete
    /// pipeline decision. A stale or no-longer-capable explicit choice falls back to
    /// Auto (never silently pretends an incapable device works).
    /// </summary>
    internal static ResolvedVideoDevice Resolve(
        CallVideoMediaDirection direction,
        string? preferenceKey,
        IReadOnlyList<NativeVideoDevice> devices)
    {
        IReadOnlyList<NativeVideoDevice> capable = devices
            .Where(device => IsCapable(device, direction))
            .ToArray();

        string key = string.IsNullOrWhiteSpace(preferenceKey) ? AutoKey : preferenceKey.Trim();

        if (string.Equals(key, SoftwareKey, StringComparison.OrdinalIgnoreCase))
        {
            // Software encode isn't a real path yet — a stale software-encode pref must not break
            // the call; fall through to Auto (which lands on a working HW encoder, else software).
            if (SoftwareSelectable(direction))
            {
                return ResolvedVideoDevice.Software("Программно (CPU)");
            }

            key = AutoKey;
        }

        if (!string.Equals(key, AutoKey, StringComparison.OrdinalIgnoreCase))
        {
            // Explicit GPU choice — honor it only if that GPU still exists and is
            // genuinely capable in this direction; otherwise drop through to Auto.
            NativeVideoDevice? match = capable.FirstOrDefault(device =>
                string.Equals(device.Description, key, StringComparison.OrdinalIgnoreCase));
            if (match is not null)
            {
                return ResolvedVideoDevice.Hardware(match.VendorId, match.Description);
            }
        }

        // Auto: energy-first — integrated working GPU, else any working GPU, else software.
        NativeVideoDevice? auto = capable.FirstOrDefault(device => device.Integrated)
            ?? capable.FirstOrDefault();
        return auto is not null
            ? ResolvedVideoDevice.Hardware(auto.VendorId, $"Авто · {auto.Description}")
            : ResolvedVideoDevice.Software("Авто · программно (CPU)");
    }
}
