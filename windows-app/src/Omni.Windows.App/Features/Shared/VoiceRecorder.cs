using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using Omni.Windows.App.Features.Calls;
using Omni.Windows.App.Presentation;

namespace Omni.Windows.App.Features.Shared;

/// What a finished recording hands the host: Opus packets, each with its length ahead, and the outline.
public sealed record VoiceRecording(byte[] Packets, byte[] Waveform);

/// A voice message being recorded: the call's own capture (noise suppression, automatic gain) and Opus
/// sender, its packets kept in memory instead of sent. Nothing reaches the disk before the person sends.
internal sealed unsafe class VoiceRecorder : IDisposable
{
    /// As in the core (`VOICE_MAX_MS`): at the limit the recording goes as it is.
    internal const int MaxMs = 15 * 60 * 1000;

    internal const int LevelIntervalMs = 100;

    /// Voice at this rate is clear and costs about 180 KB a minute.
    private const int BitrateBps = 24_000;

    /// Fewer than this is a click, not a message.
    private const int MinMs = 500;

    private const int RtpHeaderBytes = 12;

    private const int WaveformBars = 64;

    /// How long pure zeros from the microphone mean it hears nothing, rather than a pause.
    private const long DeafAfterMs = 2_000;

    /// A call on, as the app knows it: there is no recording during one.
    internal static Func<bool>? CallIsOn { get; set; }

    /// The microphone chosen in the settings.
    internal static Func<string?>? MicrophoneId { get; set; }

    private readonly NativeWebRtcMediaEngine.AudioCaptureSession capture;
    private readonly List<byte[]> packets = [];
    private readonly List<float> levels = [];
    private readonly long startedAtMs = Environment.TickCount64;
    private GCHandle self;

    private VoiceRecorder(NativeWebRtcMediaEngine.AudioCaptureSession capture)
    {
        this.capture = capture;
        self = GCHandle.Alloc(this);
        capture.SetPacketSink((IntPtr)(delegate* unmanaged[Cdecl]<IntPtr, byte*, nuint, void>)&OnPacket, GCHandle.ToIntPtr(self));
    }

    internal static bool TryStart(out VoiceRecorder? recorder, out string? why)
    {
        recorder = null;
        why = null;
        if (CallIsOn?.Invoke() == true)
        {
            why = "Во время звонка голосовое не записать.";
            return false;
        }

        if (!NativeWebRtcMediaEngine.TryCreateAudioCapture(BitrateBps, out NativeWebRtcMediaEngine.AudioCaptureSession? capture, out string diagnostics, MicrophoneId?.Invoke()) ||
            capture is null)
        {
            Core.AppTrace.Write("voice", $"capture failed: {diagnostics}");
            why = "Микрофон недоступен: проверьте, что он подключён и разрешён в Windows.";
            return false;
        }

        recorder = new VoiceRecorder(capture);
        return true;
    }

    internal int ElapsedMs => (int)Math.Min(Environment.TickCount64 - startedAtMs, int.MaxValue);

    internal bool HearsNothing => capture.ReadDigitalSilenceMs() >= DeafAfterMs;

    internal int PacketCount
    {
        get
        {
            lock (packets)
            {
                return packets.Count;
            }
        }
    }

    internal void SampleLevel() => levels.Add(capture.ReadLevel());

    /// The latest levels, as bars for the strip over the field.
    internal byte[] RecentBars()
    {
        int shown = Math.Min(levels.Count, WaveformBars);
        return levels.Skip(levels.Count - shown).Select(level => (byte)Math.Clamp((int)(255 * Math.Sqrt(level) * 1.6), 0, 255)).ToArray();
    }

    /// The recording as it stands, the microphone closed. `null`: too short to be a message.
    internal VoiceRecording? Finish()
    {
        int elapsed = ElapsedMs;
        Dispose();
        lock (packets)
        {
            return elapsed < MinMs || packets.Count == 0 ? null : new VoiceRecording(Join(packets), MainWindowPresentation.BuildVoiceWaveform(levels, WaveformBars));
        }
    }

    public void Dispose()
    {
        if (!self.IsAllocated)
        {
            return;
        }

        // After this no call into the sink is in flight (the native side holds its lock for them).
        capture.SetPacketSink(IntPtr.Zero, IntPtr.Zero);
        capture.Dispose();
        self.Free();
    }

    private static byte[] Join(List<byte[]> packets)
    {
        byte[] joined = new byte[packets.Sum(packet => packet.Length + 2)];
        int at = 0;
        foreach (byte[] packet in packets)
        {
            joined[at++] = (byte)(packet.Length & 0xFF);
            joined[at++] = (byte)(packet.Length >> 8);
            packet.CopyTo(joined, at);
            at += packet.Length;
        }

        return joined;
    }

    // The capture thread, one RTP packet at a time: the Opus payload is what stays.
    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    private static void OnPacket(IntPtr context, byte* packet, nuint length)
    {
        if (length <= RtpHeaderBytes || GCHandle.FromIntPtr(context).Target is not VoiceRecorder recorder)
        {
            return;
        }

        byte[] payload = new ReadOnlySpan<byte>(packet + RtpHeaderBytes, (int)length - RtpHeaderBytes).ToArray();
        lock (recorder.packets)
        {
            recorder.packets.Add(payload);
        }
    }

    /// Test channel: what a microphone would have given, for machines with none: a voice-like tone that
    /// swells and fades, so the outline has a shape.
    internal static VoiceRecording? ToneRecording(double seconds)
    {
        int samples = (int)(seconds * 48_000);
        short[] pcm = new short[samples];
        float[] envelope = new float[WaveformBars];
        for (int i = 0; i < samples; i++)
        {
            double t = i / 48_000d;
            double swell = 0.15 + 0.85 * Math.Abs(Math.Sin(Math.PI * t * 1.3));
            pcm[i] = (short)(9000 * swell * Math.Sin(2 * Math.PI * 330 * t));
            int bar = (int)((long)i * WaveformBars / samples);
            envelope[bar] = Math.Max(envelope[bar], (float)swell);
        }

        return NativeWebRtcMediaEngine.TryEncodeOpusPcm(pcm, BitrateBps, out byte[] packed)
            ? new VoiceRecording(packed, MainWindowPresentation.BuildVoiceWaveform(envelope, WaveformBars))
            : null;
    }
}
