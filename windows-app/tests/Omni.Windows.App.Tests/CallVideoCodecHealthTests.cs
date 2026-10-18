using Omni.Windows.App.Features.Calls;
using Xunit;

namespace Omni.Windows.App.Tests;

// docs/decisions/per-pair-video-codec-selection.md §6: a codec that broke mid-call leaves the
// passport until the call ends, and H.264 — the floor — never does.
public sealed class CallVideoCodecHealthTests
{
    private static readonly IReadOnlyList<CallVideoCodecSupport> Codecs =
    [
        new(CallVideoCodecKind.Av1, 45, EncodeHardware: true, DecodeHardware: false, "av1"),
        new(CallVideoCodecKind.H265, 49, EncodeHardware: true, DecodeHardware: true, "h265"),
        new(CallVideoCodecKind.H264, 102, EncodeHardware: true, DecodeHardware: true, "h264"),
    ];

    [Fact]
    public void A_failed_hevc_encoder_leaves_only_its_encode_line()
    {
        CallVideoCodecHealth health = new();
        Assert.False(health.ObserveEncoder(CallVideoCodecKind.H265, fallbackRequests: 0));
        Assert.True(health.ObserveEncoder(CallVideoCodecKind.H265, fallbackRequests: 1));
        // Said once: the next tick with the counter still up is not a new failure.
        Assert.False(health.ObserveEncoder(CallVideoCodecKind.H265, fallbackRequests: 5));

        CallVideoCodecSupport hevc = Assert.Single(health.Apply(Codecs), c => c.Kind == CallVideoCodecKind.H265);
        Assert.Null(hevc.EncodeHardware);
        Assert.True(hevc.DecodeHardware); // still watches HEVC from the peer
    }

    [Fact]
    public void H264_is_never_struck()
    {
        CallVideoCodecHealth health = new();
        Assert.False(health.ObserveEncoder(CallVideoCodecKind.H264, fallbackRequests: 3));
        Assert.False(health.ObserveDecoder(CallVideoCodecKind.H264, 0, 0));
        Assert.False(health.ObserveDecoder(CallVideoCodecKind.H264, 0, 500));
        Assert.Same(Codecs, health.Apply(Codecs));
    }

    [Fact]
    public void A_decoder_is_struck_only_after_a_run_of_failures_with_nothing_decoded()
    {
        CallVideoCodecHealth health = new();
        Assert.False(health.ObserveDecoder(CallVideoCodecKind.Av1, framesDecoded: 100, decodeFailures: 2));
        // A lost key frame: a few failures, then pictures again.
        Assert.False(health.ObserveDecoder(CallVideoCodecKind.Av1, 100, 20));
        Assert.False(health.ObserveDecoder(CallVideoCodecKind.Av1, 130, 25));
        Assert.False(health.ObserveDecoder(CallVideoCodecKind.Av1, 130, 25 + CallVideoCodecHealth.DecodeFailuresWithoutProgress - 1));
        Assert.True(health.ObserveDecoder(CallVideoCodecKind.Av1, 130, 25 + CallVideoCodecHealth.DecodeFailuresWithoutProgress));

        // The peer may no longer send AV1 here; encoding it is untouched.
        Assert.DoesNotContain(health.Apply(Codecs), c => c.Kind == CallVideoCodecKind.Av1 && c.DecodeHardware is not null);
    }

    [Fact]
    public void A_rebuilt_receiver_counts_from_its_own_zero()
    {
        CallVideoCodecHealth health = new();
        Assert.False(health.ObserveDecoder(CallVideoCodecKind.H265, 900, 50));
        // The receiver was rebuilt (a new selection): its counters start over.
        Assert.False(health.ObserveDecoder(CallVideoCodecKind.H265, 0, 0));
        Assert.False(health.ObserveDecoder(CallVideoCodecKind.H265, 40, 3));
        Assert.Empty(health.FailedDecode);
    }

    [Fact]
    public void An_encoder_that_takes_frames_and_puts_none_out_is_struck()
    {
        CallVideoCodecHealth health = new();
        // Warming up: a few frames in before the first comes out is normal.
        Assert.False(health.ObserveEncoderProgress(CallVideoCodecKind.H265, 10, 0));
        Assert.False(health.ObserveEncoderProgress(CallVideoCodecKind.H265, 40, 5));
        // Then it stops putting anything out (stand, 2026-10-14: NVIDIA HEVC MFT on battery).
        Assert.False(health.ObserveEncoderProgress(CallVideoCodecKind.H265, 40 + CallVideoCodecHealth.EncoderFramesWithoutOutput - 1, 5));
        Assert.True(health.ObserveEncoderProgress(CallVideoCodecKind.H265, 40 + CallVideoCodecHealth.EncoderFramesWithoutOutput, 5));
        Assert.Contains(CallVideoCodecKind.H265, health.FailedEncode);
        // News once.
        Assert.False(health.ObserveEncoderProgress(CallVideoCodecKind.H265, 1000, 5));
    }

    [Fact]
    public void A_rebuilt_sender_counts_from_its_own_zero_and_h264_is_never_struck_for_stalling()
    {
        CallVideoCodecHealth health = new();
        Assert.False(health.ObserveEncoderProgress(CallVideoCodecKind.Av1, 500, 480));
        Assert.False(health.ObserveEncoderProgress(CallVideoCodecKind.Av1, 0, 0));
        Assert.False(health.ObserveEncoderProgress(CallVideoCodecKind.Av1, 60, 55));
        Assert.False(health.ObserveEncoderProgress(CallVideoCodecKind.H264, 1000, 55));
        Assert.Empty(health.FailedEncode);
    }

    [Fact]
    public void A_new_call_gives_every_codec_its_chance_again()
    {
        CallVideoCodecHealth health = new();
        health.ObserveEncoder(CallVideoCodecKind.Av1, 1);
        health.Reset();
        Assert.Empty(health.FailedEncode);
        Assert.Same(Codecs, health.Apply(Codecs));
    }
}
