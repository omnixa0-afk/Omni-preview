using Microsoft.UI.Dispatching;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Calls;
using Windows.Media.Core;
using Windows.Media.Playback;
using Windows.Storage.Streams;

namespace Omni.Windows.App.Features.Shared;

/// The one voice message playing, whichever conversation it is in: another one started stops it. The
/// sound is decoded by our own Opus (a Windows without the codec plays it all the same), from what the
/// core read and checked, into memory: the file on the disk stays encrypted.
internal static class VoicePlayer
{
    /// Voice needs no more, and a 15-minute message stays about 40 MB in memory while it plays.
    private const int SampleRateHz = 24_000;

    /// Room for the length the file says, and a margin for what a decoder conceals.
    private const int MarginSamples = SampleRateHz * 2;

    private static MediaPlayer? player;
    private static readonly PlaybackClock clock = new();
    private static bool ticking;
    private static MessageRow? current;
    private static string? currentId;
    private static int decodedSamples;
    private static int peakSample;
    private static bool loading;
    private static double? seekWhenOpen;
    private static string lastError = string.Empty;

    /// Rows are rebuilt as a conversation refreshes: the new one takes over the playing state.
    internal static void Reattach(MessageRow row)
    {
        if (row.MessageId == currentId && !ReferenceEquals(row, current))
        {
            current = row;
            Publish();
        }
    }

    internal static void Toggle(MessageRow row)
    {
        if (row.MessageId != currentId)
        {
            _ = StartAsync(row, null);
            return;
        }

        current = row;
        if (loading || player is not { } active)
        {
            return;
        }

        if (active.PlaybackSession.PlaybackState == MediaPlaybackState.Playing)
        {
            active.Pause();
            SetTicking(false);
        }
        else
        {
            active.Play();
            SetTicking(true);
        }

        Publish();
    }

    /// A tap on the outline: that moment. A message not playing starts there.
    internal static void Seek(MessageRow row, double fraction)
    {
        fraction = Math.Clamp(fraction, 0, 1);
        if (row.MessageId != currentId)
        {
            _ = StartAsync(row, fraction);
            return;
        }

        if (loading || player is not { } active)
        {
            seekWhenOpen = fraction;
            return;
        }

        active.PlaybackSession.Position = TimeSpan.FromTicks((long)(active.PlaybackSession.NaturalDuration.Ticks * fraction));
        Publish();
    }

    internal static string StateForUiTest()
    {
        MediaPlaybackSession? session = player?.PlaybackSession;
        return $"id={currentId ?? "none"} loading={loading} state={session?.PlaybackState} " +
               $"position_ms={(int)(session?.Position.TotalMilliseconds ?? 0)} length_ms={(int)(session?.NaturalDuration.TotalMilliseconds ?? 0)} " +
               $"samples={decodedSamples} peak={peakSample} error={lastError}";
    }

    private static async Task StartAsync(MessageRow row, double? seek)
    {
        Stop();
        VideoNotePlayer.Stop();
        string messageId = row.MessageId;
        string? path = row.AttachmentLocalPath;
        current = row;
        currentId = messageId;
        loading = true;
        seekWhenOpen = seek;
        lastError = string.Empty;
        row.SetVoiceState(false, true, seek ?? 0, 0);
        byte[]? wave = path is null ? null : await Task.Run(() => DecodeToWave(path));
        if (currentId != messageId)
        {
            return; // Another one was started meanwhile.
        }

        loading = false;
        if (wave is null)
        {
            Stop();
            return;
        }

        InMemoryRandomAccessStream stream = await MediaFiles.ToStreamAsync(wave);
        if (currentId != messageId)
        {
            stream.Dispose();
            return;
        }

        MediaPlayer next = new() { AudioCategory = MediaPlayerAudioCategory.Media };
        DispatcherQueue queue = DispatcherQueue.GetForCurrentThread();
        next.MediaEnded += (_, _) => queue.TryEnqueue(() => { if (ReferenceEquals(player, next)) { Stop(); } });
        next.MediaFailed += (_, failed) => queue.TryEnqueue(() =>
        {
            if (ReferenceEquals(player, next))
            {
                lastError = failed.ErrorMessage;
                AppTrace.Write("voice", $"playback failed: {failed.Error} {failed.ErrorMessage}");
                Stop();
            }
        });
        next.MediaOpened += (_, _) => queue.TryEnqueue(() =>
        {
            if (ReferenceEquals(player, next) && seekWhenOpen is { } fraction)
            {
                seekWhenOpen = null;
                next.PlaybackSession.Position = TimeSpan.FromTicks((long)(next.PlaybackSession.NaturalDuration.Ticks * fraction));
            }
        });
        player = next;
        next.Source = MediaSource.CreateFromStream(stream, "audio/wav");
        next.Play();

        SetTicking(true);
        Publish();
    }

    // Every frame while it plays, so the line moves as smoothly as the screen can show it; a paused
    // player asks for no frames.
    private static void SetTicking(bool on)
    {
        if (on == ticking)
        {
            return;
        }

        ticking = on;
        if (on)
        {
            Microsoft.UI.Xaml.Media.CompositionTarget.Rendering += OnFrame;
        }
        else
        {
            Microsoft.UI.Xaml.Media.CompositionTarget.Rendering -= OnFrame;
        }
    }

    private static void OnFrame(object? sender, object e) => Publish();

    private static void Publish()
    {
        if (current is not { } row)
        {
            return;
        }

        if (player is not { } active)
        {
            row.SetVoiceState(false, loading, 0, 0);
            return;
        }

        MediaPlaybackSession session = active.PlaybackSession;
        double length = session.NaturalDuration.TotalMilliseconds;
        bool playing = session.PlaybackState is MediaPlaybackState.Playing or MediaPlaybackState.Opening or MediaPlaybackState.Buffering;
        double position = clock.Read(session.Position.TotalMilliseconds, session.PlaybackState == MediaPlaybackState.Playing);
        double progress = length > 0 ? Math.Clamp(position / length, 0, 1) : 0;
        row.SetVoiceState(playing, false, progress, (int)position);
    }

    internal static void Stop()
    {
        SetTicking(false);
        MediaPlayer? stopping = player;
        player = null;
        stopping?.Pause();
        stopping?.Dispose();
        current?.SetVoiceState(false, false, 0, 0);
        current = null;
        currentId = null;
        loading = false;
        seekWhenOpen = null;
    }

    private static byte[]? DecodeToWave(string path)
    {
        try
        {
            (byte[] packets, int durationMs) = MediaFiles.ReadVoice(path);
            int capacity = (int)(durationMs * (long)SampleRateHz / 1000) + MarginSamples;
            if (!NativeWebRtcMediaEngine.TryDecodeOpusPackets(packets, SampleRateHz, capacity, out short[] pcm, out int samples) || samples == 0)
            {
                lastError = "decoder";
                AppTrace.Write("voice", "decoding failed");
                return null;
            }

            decodedSamples = samples;
            peakSample = 0;
            for (int i = 0; i < samples; i++)
            {
                peakSample = Math.Max(peakSample, Math.Abs((int)pcm[i]));
            }

            return BuildWave(pcm, samples, SampleRateHz);
        }
        catch (InvalidOperationException ex)
        {
            // The core said why (the bridge wrote the diagnostic): nothing plays.
            lastError = ex.Message;
            return null;
        }
    }

    /// 16-bit mono PCM in a WAV header: what the system player takes with no codec at all.
    internal static byte[] BuildWave(short[] pcm, int samples, int sampleRateHz)
    {
        int dataBytes = samples * 2;
        byte[] wave = new byte[44 + dataBytes];
        using MemoryStream memory = new(wave);
        using BinaryWriter writer = new(memory);
        writer.Write("RIFF"u8);
        writer.Write(36 + dataBytes);
        writer.Write("WAVEfmt "u8);
        writer.Write(16);
        writer.Write((short)1); // PCM
        writer.Write((short)1); // mono
        writer.Write(sampleRateHz);
        writer.Write(sampleRateHz * 2);
        writer.Write((short)2);
        writer.Write((short)16);
        writer.Write("data"u8);
        writer.Write(dataBytes);
        System.Buffer.BlockCopy(pcm, 0, wave, 44, dataBytes);
        return wave;
    }
}
