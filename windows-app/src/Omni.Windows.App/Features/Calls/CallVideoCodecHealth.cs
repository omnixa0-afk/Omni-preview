namespace Omni.Windows.App.Features.Calls;

/// <summary>
/// A codec that broke in the middle of a 1:1 call (docs/decisions/per-pair-video-codec-selection.md §6).
/// HEVC has no software encoder or decoder to fall back to, so a driver that drops the hardware
/// session mid-call used to leave that direction without a picture for the rest of the call. Now
/// the broken side leaves the codec out of its passport until the call ends, the passport is
/// announced again, and the pair moves to the next codec they share.
///
/// H.264 is never struck: it is the floor every pair falls back to, and its own encoder already
/// hands over to the software one inside the native session.
/// </summary>
internal sealed class CallVideoCodecHealth
{
    /// <summary>
    /// Decode failures in a row, with not one frame decoded in between, that mean "this codec does
    /// not decode here" rather than a lost packet: about two seconds of video at 30 frames a
    /// second. A late join or a lost key frame fails a handful of frames and then decodes again.
    /// </summary>
    internal const uint DecodeFailuresWithoutProgress = 60;

    /// <summary>
    /// Frames handed to the encoder, with not one frame out, that mean "this encoder is stuck"
    /// rather than "it is warming up": about three seconds of camera. A hardware encoder that
    /// takes frames and answers nothing never fails, so the fallback counter does not see it
    /// (stand, 2026-10-14: the laptop's Intel QSV HEVC MFT, 407 frames in, 0 out, a call without a
    /// picture until it ended; docs/bugs/a-stuck-encoder-was-never-struck.md).
    /// </summary>
    internal const uint EncoderFramesWithoutOutput = 90;

    private readonly HashSet<CallVideoCodecKind> failedEncode = [];
    private readonly HashSet<CallVideoCodecKind> failedDecode = [];
    private uint decodedSeen;
    private uint failuresAtLastProgress;
    private uint encodedSeen;
    private uint incomingAtLastOutput;

    internal IReadOnlyCollection<CallVideoCodecKind> FailedEncode => failedEncode;

    internal IReadOnlyCollection<CallVideoCodecKind> FailedDecode => failedDecode;

    /// <summary>A new call: every codec gets its chance again.</summary>
    internal void Reset()
    {
        failedEncode.Clear();
        failedDecode.Clear();
        decodedSeen = 0;
        failuresAtLastProgress = 0;
        encodedSeen = 0;
        incomingAtLastOutput = 0;
    }

    /// <summary>
    /// The sender's hardware encoder asked to be replaced (the native session counts it after
    /// three failures in a row). True the first time it happens to this codec in this call.
    /// </summary>
    internal bool ObserveEncoder(CallVideoCodecKind codec, uint fallbackRequests)
        => codec != CallVideoCodecKind.H264 && fallbackRequests > 0 && failedEncode.Add(codec);

    /// <summary>
    /// The sender's frame counters, sampled once a second. True the first time this codec took
    /// <see cref="EncoderFramesWithoutOutput"/> frames without putting one out.
    /// </summary>
    internal bool ObserveEncoderProgress(CallVideoCodecKind codec, uint framesIn, uint framesOut)
    {
        // A rebuilt sender starts its counters from zero: count from there.
        if (framesIn < incomingAtLastOutput || framesOut < encodedSeen)
        {
            encodedSeen = framesOut;
            incomingAtLastOutput = framesIn;
            return false;
        }

        if (framesOut > encodedSeen)
        {
            encodedSeen = framesOut;
            incomingAtLastOutput = framesIn;
            return false;
        }

        return framesIn - incomingAtLastOutput >= EncoderFramesWithoutOutput &&
               codec != CallVideoCodecKind.H264 &&
               failedEncode.Add(codec);
    }

    /// <summary>
    /// The receiver's counters, sampled once a second. True the first time this codec failed
    /// <see cref="DecodeFailuresWithoutProgress"/> frames without decoding one.
    /// </summary>
    internal bool ObserveDecoder(CallVideoCodecKind codec, uint framesDecoded, uint decodeFailures)
    {
        // A rebuilt receiver starts its counters from zero: count from there.
        if (framesDecoded < decodedSeen || decodeFailures < failuresAtLastProgress)
        {
            decodedSeen = framesDecoded;
            failuresAtLastProgress = decodeFailures;
            return false;
        }

        if (framesDecoded > decodedSeen)
        {
            decodedSeen = framesDecoded;
            failuresAtLastProgress = decodeFailures;
            return false;
        }

        return decodeFailures - failuresAtLastProgress >= DecodeFailuresWithoutProgress &&
               StrikeDecoder(codec);
    }

    /// <summary>This codec no longer decodes here for the rest of the call. True when it is news.</summary>
    internal bool StrikeDecoder(CallVideoCodecKind codec)
        => codec != CallVideoCodecKind.H264 && failedDecode.Add(codec);

    /// <summary>The passport's codec lines without what broke in this call.</summary>
    internal IReadOnlyList<CallVideoCodecSupport> Apply(IReadOnlyList<CallVideoCodecSupport> codecs)
    {
        if (failedEncode.Count == 0 && failedDecode.Count == 0)
        {
            return codecs;
        }

        List<CallVideoCodecSupport> kept = [];
        foreach (CallVideoCodecSupport codec in codecs)
        {
            bool? encode = failedEncode.Contains(codec.Kind) ? null : codec.EncodeHardware;
            bool? decode = failedDecode.Contains(codec.Kind) ? null : codec.DecodeHardware;
            if (encode is null && decode is null)
            {
                continue;
            }

            kept.Add(codec with { EncodeHardware = encode, DecodeHardware = decode });
        }

        return kept;
    }
}
