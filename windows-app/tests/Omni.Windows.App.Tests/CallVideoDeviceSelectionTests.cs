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
