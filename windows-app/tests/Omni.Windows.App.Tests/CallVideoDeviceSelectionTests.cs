using System.Collections.Generic;
using System.Linq;
using Omni.Windows.App.Features.Calls;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class CallVideoDeviceSelectionTests
{
    // The real 2-GPU laptop matrix the native probe reports: the discrete NVIDIA can
    // both encode and decode; the integrated Intel can only decode (QSV encode fails).
    private static readonly NativeVideoDevice Nvidia = new(
        "NVIDIA GeForce RTX 4060 Laptop GPU", 0x10DE, Integrated: false, CanEncodeH264: true, CanDecodeH264: true);

    private static readonly NativeVideoDevice IntelIgpu = new(
        "Intel(R) Iris(R) Xe Graphics", 0x8086, Integrated: true, CanEncodeH264: false, CanDecodeH264: true);

    private static IReadOnlyList<NativeVideoDevice> TwoGpu => [Nvidia, IntelIgpu];

    [Fact]
    public void Decode_Auto_PrefersIntegratedCapableGpu()
    {
        ResolvedVideoDevice resolved = CallVideoDeviceSelection.Resolve(
            CallVideoMediaDirection.Decode, CallVideoDeviceSelection.AutoKey, TwoGpu);

        Assert.False(resolved.UseSoftware);
        Assert.Equal(0x8086u, resolved.VendorId); // Intel iGPU — energy-first
    }

    [Fact]
    public void Encode_Auto_FallsToOnlyCapableHardware_EvenIfDiscrete()
    {
        // Intel can't actually encode, so Auto must pick the discrete NVIDIA, not software.
        ResolvedVideoDevice resolved = CallVideoDeviceSelection.Resolve(
            CallVideoMediaDirection.Encode, CallVideoDeviceSelection.AutoKey, TwoGpu);

        Assert.False(resolved.UseSoftware);
        Assert.Equal(0x10DEu, resolved.VendorId); // NVIDIA — only working encoder
    }

    [Fact]
    public void Decode_SoftwareKey_ForcesSoftware()
    {
        ResolvedVideoDevice resolved = CallVideoDeviceSelection.Resolve(
            CallVideoMediaDirection.Decode, CallVideoDeviceSelection.SoftwareKey, TwoGpu);

        Assert.True(resolved.UseSoftware);
        Assert.Equal(0u, resolved.VendorId);
    }

    [Fact]
    public void Encode_SoftwareKey_FallsBackToAuto_BecauseSoftwareEncodeIsNotWired()
    {
        // Software encode isn't a real path yet, so a software-encode pref must not stick —
        // it falls through to Auto, which lands on the only working encoder (NVIDIA).
        ResolvedVideoDevice resolved = CallVideoDeviceSelection.Resolve(
            CallVideoMediaDirection.Encode, CallVideoDeviceSelection.SoftwareKey, TwoGpu);

        Assert.False(resolved.UseSoftware);
        Assert.Equal(0x10DEu, resolved.VendorId);
    }

    [Fact]
    public void Explicit_CapableDevice_IsHonored()
    {
        // User explicitly picks the discrete GPU for decode (e.g. max quality).
        ResolvedVideoDevice resolved = CallVideoDeviceSelection.Resolve(
            CallVideoMediaDirection.Decode, Nvidia.Description, TwoGpu);

        Assert.False(resolved.UseSoftware);
        Assert.Equal(0x10DEu, resolved.VendorId);
    }

    [Fact]
    public void Explicit_IncapableDevice_FallsBackToAuto()
    {
        // User's stored choice is the Intel iGPU for ENCODE, which it can't do.
        // Must not pretend it works — fall back to Auto (-> NVIDIA, the only encoder).
        ResolvedVideoDevice resolved = CallVideoDeviceSelection.Resolve(
            CallVideoMediaDirection.Encode, IntelIgpu.Description, TwoGpu);

        Assert.False(resolved.UseSoftware);
        Assert.Equal(0x10DEu, resolved.VendorId);
    }

    [Fact]
    public void Explicit_MissingDevice_FallsBackToAuto()
    {
        ResolvedVideoDevice resolved = CallVideoDeviceSelection.Resolve(
            CallVideoMediaDirection.Decode, "Some Unplugged eGPU", TwoGpu);

        Assert.False(resolved.UseSoftware);
        Assert.Equal(0x8086u, resolved.VendorId); // Auto -> integrated
    }

    [Fact]
    public void Decode_NoCapableHardware_ResolvesToSoftware()
    {
        // A device that can neither encode nor decode -> Auto-decode honestly lands on software.
        NativeVideoDevice noCodec = new("Render-only display adapter", 0x1234,
            Integrated: true, CanEncodeH264: false, CanDecodeH264: false);
        ResolvedVideoDevice resolved = CallVideoDeviceSelection.Resolve(
            CallVideoMediaDirection.Decode, CallVideoDeviceSelection.AutoKey, [noCodec]);

        Assert.True(resolved.UseSoftware);
    }

    [Fact]
    public void BuildOptions_Encode_OmitsNonEncodingGpu_AndHasNoSoftware()
    {
        IReadOnlyList<CallVideoDeviceOption> options = CallVideoDeviceSelection.BuildOptions(
            CallVideoMediaDirection.Encode, TwoGpu);

        Assert.Equal(CallVideoDeviceSelection.AutoKey, options[0].Key);
        Assert.DoesNotContain(options, option => option.Key == CallVideoDeviceSelection.SoftwareKey);
        Assert.Contains(options, option => option.Key == Nvidia.Description);
        Assert.DoesNotContain(options, option => option.Key == IntelIgpu.Description);
    }

    [Fact]
    public void BuildOptions_Decode_HasSoftwareAndBothCapableGpus()
    {
        IReadOnlyList<CallVideoDeviceOption> options = CallVideoDeviceSelection.BuildOptions(
            CallVideoMediaDirection.Decode, TwoGpu);

        Assert.Equal(CallVideoDeviceSelection.AutoKey, options[0].Key);
        Assert.Equal(CallVideoDeviceSelection.SoftwareKey, options[1].Key);
        Assert.Contains(options, option => option.Key == Nvidia.Description);
        Assert.Contains(options, option => option.Key == IntelIgpu.Description);
    }

    [Fact]
    public void BuildOptions_Encode_NoDevices_IsAutoOnly()
    {
        IReadOnlyList<CallVideoDeviceOption> options = CallVideoDeviceSelection.BuildOptions(
            CallVideoMediaDirection.Encode, []);

        Assert.Single(options);
        Assert.Equal(CallVideoDeviceSelection.AutoKey, options[0].Key);
    }

    // Today's laptop: both cards encode H.264, but HEVC and AV1 were proven on the NVIDIA only.
    private static readonly NativeVideoDevice IntelUhd = new(
        "Intel(R) UHD Graphics", 0x8086, Integrated: true, CanEncodeH264: true, CanDecodeH264: true);

    private static readonly Dictionary<string, uint> VerifiedOnNvidia = new()
    {
        ["h264"] = 0x10DE,
        ["h265"] = 0x10DE,
        ["av1"] = 0x10DE,
    };

    [Fact]
    public void ExplicitIntel_IsMarkedExplicit_AndEncodesOnlyH264()
    {
        ResolvedVideoDevice resolved = CallVideoDeviceSelection.Resolve(
            CallVideoMediaDirection.Encode, IntelUhd.Description, [Nvidia, IntelUhd]);

        Assert.True(resolved.Explicit);
        Assert.Equal(0x8086u, resolved.VendorId);
        Assert.Equal(
            [CallVideoCodecKind.H264],
            CallVideoDeviceSelection.HardwareEncodeCodecs(resolved.VendorId, canEncodeH264: true, VerifiedOnNvidia));
        Assert.Equal(
            [CallVideoCodecKind.H264, CallVideoCodecKind.H265, CallVideoCodecKind.Av1],
            CallVideoDeviceSelection.HardwareEncodeCodecs(0x10DE, canEncodeH264: true, VerifiedOnNvidia));
    }

    [Fact]
    public void Auto_IsNotExplicit_SoNothingNarrowsThePassport()
    {
        ResolvedVideoDevice resolved = CallVideoDeviceSelection.Resolve(
            CallVideoMediaDirection.Encode, CallVideoDeviceSelection.AutoKey, [Nvidia, IntelUhd]);

        Assert.False(resolved.Explicit);
        Assert.Equal(0x8086u, resolved.VendorId); // energy-first for H.264
    }

    [Fact]
    public void BuildOptions_Encode_NamesTheCodecsEachCardEncodes()
    {
        IReadOnlyList<CallVideoDeviceOption> options = CallVideoDeviceSelection.BuildOptions(
            CallVideoMediaDirection.Encode, [Nvidia, IntelUhd], VerifiedOnNvidia);

        Assert.Equal(
            "NVIDIA GeForce RTX 4060 Laptop GPU · дискретная · H.264, HEVC, AV1",
            options.Single(option => option.Key == Nvidia.Description).DisplayName);
        Assert.Equal(
            "Intel(R) UHD Graphics · встроенная · H.264",
            options.Single(option => option.Key == IntelUhd.Description).DisplayName);
    }

    [Fact]
    public void ExplicitCard_PassportDropsAnotherCardsCodecs()
    {
        IReadOnlyList<CallVideoCodecSupport> all =
        [
            new CallVideoCodecSupport(CallVideoCodecKind.Av1, CallVideoRtpConstants.Av1PayloadType, true, true, "av1"),
            new CallVideoCodecSupport(CallVideoCodecKind.H265, CallVideoRtpConstants.H265PayloadType, true, true, "h265"),
            new CallVideoCodecSupport(CallVideoCodecKind.H264, CallVideoRtpConstants.H264PayloadType, true, true, "h264"),
        ];

        IReadOnlyList<CallVideoCodecSupport> onIntel = CallVideoCodecFactory.RestrictToDevices(
            all, encodeSoftwareOnly: false, decodeSoftwareOnly: false, [CallVideoCodecKind.H264]);

        // H.264 stays hardware on the chosen card; HEVC is not encoded at all (receive only); AV1
        // falls to software, which the core takes only when no hardware path exists (H.264 is one).
        Assert.Equal(true, onIntel.Single(c => c.Kind == CallVideoCodecKind.H264).EncodeHardware);
        Assert.Null(onIntel.Single(c => c.Kind == CallVideoCodecKind.H265).EncodeHardware);
        Assert.Equal(true, onIntel.Single(c => c.Kind == CallVideoCodecKind.H265).DecodeHardware);
        Assert.Equal(false, onIntel.Single(c => c.Kind == CallVideoCodecKind.Av1).EncodeHardware);
    }

    [Fact]
    public void BuildOptions_Decode_NoDevices_IsAutoAndSoftwareOnly()
    {
        IReadOnlyList<CallVideoDeviceOption> options = CallVideoDeviceSelection.BuildOptions(
            CallVideoMediaDirection.Decode, []);

        Assert.Equal(2, options.Count);
        Assert.Equal(CallVideoDeviceSelection.AutoKey, options[0].Key);
        Assert.Equal(CallVideoDeviceSelection.SoftwareKey, options[1].Key);
    }
}
