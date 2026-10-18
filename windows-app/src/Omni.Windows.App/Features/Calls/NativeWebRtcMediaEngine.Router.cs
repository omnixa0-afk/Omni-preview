using System.Runtime.InteropServices;

namespace Omni.Windows.App.Features.Calls;

internal static partial class NativeWebRtcMediaEngine
{
    /// <summary>
    /// The native inbound-media router of one call (omni_webrtc_media.h, ABI 14). The core hands
    /// every inbound datagram of the call straight to it (<see cref="DeliverFunction"/> with
    /// <see cref="Handle"/> as context, wired through the core's inbound sink); it routes the way
    /// the shell used to — the delivery report to the sender session, video to the receiver, audio
    /// to the playback, group media by sender tag. The shell only tells it which sessions exist and
    /// builds the ones it asks for (<see cref="TakeRequests"/>). See
    /// docs/bugs/media-rode-the-shells-timer.md.
    /// </summary>
    /// <remarks>
    /// Every setter returns only when no delivery is in flight, so a session detached here can be
    /// disposed right after. Unwire the core's inbound sink BEFORE disposing the router.
    /// </remarks>
    internal sealed class MediaRouter : IDisposable
    {
        internal const int GroupSenderTagBytes = 8;
        private const int MaxWantedGroupSenders = 16;

        private readonly NativeWebRtcLibrary library;
        private IntPtr handle;

        private MediaRouter(NativeWebRtcLibrary library, IntPtr handle)
        {
            this.library = library;
            this.handle = handle;
        }

        ~MediaRouter()
        {
            Dispose();
        }

        internal IntPtr Handle => handle;

        /// <summary>The native entry the core calls per inbound datagram, with <see cref="Handle"/> as context.</summary>
        internal IntPtr DeliverFunction => library.RouterDeliver;

        internal static bool TryCreate(out MediaRouter? router, out string diagnostics)
        {
            router = null;
            NativeLibraryLoadResult loadResult = Library.Value;
            if (loadResult.Library is null)
            {
                diagnostics = loadResult.Diagnostics;
                return false;
            }

            NativeWebRtcStatus status = loadResult.Library.RouterCreate(out IntPtr created);
            if (status != NativeWebRtcStatus.Ok || created == IntPtr.Zero)
            {
                diagnostics = $"router_create failed: {status}";
                return false;
            }

            router = new MediaRouter(loadResult.Library, created);
            diagnostics = "router=ready";
            return true;
        }

        internal void SetSender(Session? sender)
        {
            ThrowIfDisposed();
            ThrowIfFailed(library.RouterSetSender(handle, sender?.Handle ?? IntPtr.Zero), "router_set_sender");
        }

        internal void SetReceiver(Session? receiver)
        {
            ThrowIfDisposed();
            ThrowIfFailed(library.RouterSetReceiver(handle, receiver?.Handle ?? IntPtr.Zero), "router_set_receiver");
        }

        internal void SetAudioPlayback(AudioPlaybackSession? playback)
        {
            ThrowIfDisposed();
            ThrowIfFailed(library.RouterSetAudioPlayback(handle, playback?.Handle ?? IntPtr.Zero), "router_set_audio_playback");
        }

        internal void SetGroupMode(bool groupMode)
        {
            ThrowIfDisposed();
            ThrowIfFailed(library.RouterSetGroupMode(handle, groupMode ? 1 : 0), "router_set_group_mode");
        }

        /// <summary>Wire one group sender's receiver and level probe; both null removes it.</summary>
        internal unsafe void SetGroupSender(ReadOnlySpan<byte> senderTag, Session? receiver, AudioPlaybackSession? levelProbe)
        {
            ThrowIfDisposed();
            if (senderTag.Length != GroupSenderTagBytes)
            {
                throw new ArgumentException($"group sender tag must be {GroupSenderTagBytes} bytes", nameof(senderTag));
            }

            fixed (byte* tag = senderTag)
            {
                ThrowIfFailed(
                    library.RouterSetGroupSender(
                        handle,
                        (IntPtr)tag,
                        receiver?.Handle ?? IntPtr.Zero,
                        levelProbe?.Handle ?? IntPtr.Zero),
                    "router_set_group_sender");
            }
        }

        /// <summary>
        /// What arrived with nobody to take it since the last call: group senders by tag, each with
        /// its latest video payload type (0 = no video yet). A sender already wired comes back here
        /// when its video arrives in a codec its receiver was not built for.
        /// </summary>
        internal unsafe (bool VideoReceiverWanted, IReadOnlyList<(byte[] Tag, byte VideoPayloadType)> GroupSenders) TakeRequests()
        {
            ThrowIfDisposed();
            NativeWebRtcMediaRouterRequestsNative requests = default;
            ThrowIfFailed(library.RouterTakeRequests(handle, &requests), "router_take_requests");
            List<(byte[], byte)> senders = [];
            int count = (int)Math.Min(requests.GroupSenderCount, (uint)MaxWantedGroupSenders);
            for (int i = 0; i < count; i++)
            {
                senders.Add((
                    new ReadOnlySpan<byte>(requests.GroupSenderTags + (i * GroupSenderTagBytes), GroupSenderTagBytes).ToArray(),
                    requests.GroupSenderVideoPayloadTypes[i]));
            }

            return (requests.VideoReceiverWanted != 0, senders);
        }

        /// <summary>Delivered by the core / of them delivery reports / dropped as malformed.</summary>
        internal (ulong Delivered, ulong Feedback, ulong Dropped) ReadCounters()
        {
            if (handle == IntPtr.Zero)
            {
                return (0, 0, 0);
            }

            return library.RouterCounters(handle, out ulong delivered, out ulong feedback, out ulong dropped) == NativeWebRtcStatus.Ok
                ? (delivered, feedback, dropped)
                : (0, 0, 0);
        }

        public void Dispose()
        {
            IntPtr current = Interlocked.Exchange(ref handle, IntPtr.Zero);
            if (current != IntPtr.Zero)
            {
                library.RouterDestroy(current);
            }

            GC.SuppressFinalize(this);
        }

        private void ThrowIfDisposed()
        {
            ObjectDisposedException.ThrowIf(handle == IntPtr.Zero, this);
        }
    }

    [StructLayout(LayoutKind.Sequential)]
    internal unsafe struct NativeWebRtcMediaRouterRequestsNative
    {
        internal int VideoReceiverWanted;
        internal uint GroupSenderCount;
        internal fixed byte GroupSenderTags[16 * 8];
        internal fixed byte GroupSenderVideoPayloadTypes[16];
    }
}
