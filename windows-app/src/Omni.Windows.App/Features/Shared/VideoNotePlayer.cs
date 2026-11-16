using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Omni.Windows.App.Core;
using Windows.Media.Core;
using Windows.Media.Playback;
using Windows.Storage.Streams;

namespace Omni.Windows.App.Features.Shared;

/// The one video message playing. A message shows only its first frame until it is pressed, so a feed of
/// them holds no decoders; pressing puts a player into its circle for as long as it plays. The bytes come
/// from the core into memory: the file on the disk stays encrypted.
internal static class VideoNotePlayer
{
    private static MediaPlayer? player;
    private static MediaPlayerElement? element;
    private static Panel? host;
    private static MessageRow? current;
    private static readonly PlaybackClock clock = new();
    private static bool ticking;
    private static bool loading;
    private static string lastError = string.Empty;

    internal static void Toggle(MessageRow row, Panel circle)
    {
        if (!ReferenceEquals(row, current))
        {
            _ = StartAsync(row, circle);
            return;
        }

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

    /// The bubble that shows it goes away or shows another message now.
    internal static void StopIf(MessageRow? row)
    {
        if (row is not null && ReferenceEquals(row, current))
        {
            Stop();
        }
    }

    internal static string StateForUiTest()
    {
        MediaPlaybackSession? session = player?.PlaybackSession;
        return $"id={current?.MessageId ?? "none"} loading={loading} state={session?.PlaybackState} " +
               $"position_ms={(int)(session?.Position.TotalMilliseconds ?? 0)} length_ms={(int)(session?.NaturalDuration.TotalMilliseconds ?? 0)} " +
               $"video={session?.NaturalVideoWidth}x{session?.NaturalVideoHeight} error={lastError}";
    }

    private static async Task StartAsync(MessageRow row, Panel circle)
    {
        Stop();
        VoicePlayer.Stop();
        if (row.AttachmentLocalPath is not { Length: > 0 } path)
        {
            return;
        }

        current = row;
        loading = true;
        lastError = string.Empty;
        row.SetVideoNoteState(false, 0, 0);
        byte[]? bytes = await Task.Run(() => MediaFiles.ReadAllBytes(path));
        if (!ReferenceEquals(row, current))
        {
            return; // Another one was started meanwhile.
        }

        loading = false;
        if (bytes is null)
        {
            lastError = "unreadable";
            Stop();
            return;
        }

        InMemoryRandomAccessStream stream = await MediaFiles.ToStreamAsync(bytes);
        if (!ReferenceEquals(row, current))
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
                AppTrace.Write("video-note", $"playback failed: {failed.Error} {failed.ErrorMessage}");
                Stop();
            }
        });
        player = next;
        host = circle;
        element = new MediaPlayerElement { Stretch = Stretch.UniformToFill, IsHitTestVisible = false };
        // Under the time and the state, over the first frame.
        circle.Children.Insert(1, element);
        element.SetMediaPlayer(next);
        next.Source = MediaSource.CreateFromStream(stream, "video/mp4");
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
        if (current is not { } row || player is not { } active)
        {
            return;
        }

        MediaPlaybackSession session = active.PlaybackSession;
        double length = session.NaturalDuration.TotalMilliseconds;
        bool playing = session.PlaybackState is MediaPlaybackState.Playing or MediaPlaybackState.Opening or MediaPlaybackState.Buffering;
        double position = clock.Read(session.Position.TotalMilliseconds, session.PlaybackState == MediaPlaybackState.Playing);
        double progress = length > 0 ? Math.Clamp(position / length, 0, 1) : 0;
        row.SetVideoNoteState(playing, progress, (int)position);
    }

    internal static void Stop()
    {
        SetTicking(false);
        MediaPlayer? stopping = player;
        player = null;
        if (element is { } shown)
        {
            shown.SetMediaPlayer(null);
            host?.Children.Remove(shown);
        }

        element = null;
        host = null;
        stopping?.Pause();
        stopping?.Dispose();
        current?.SetVideoNoteState(false, 0, 0);
        current = null;
        loading = false;
    }
}
