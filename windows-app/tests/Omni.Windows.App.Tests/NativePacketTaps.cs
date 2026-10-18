using System.Collections.Concurrent;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using Omni.Windows.App.Features.Calls;

namespace Omni.Windows.App.Tests;

/// <summary>
/// Collects what a native session hands out, for tests that want the packets back in a batch.
/// </summary>
/// <remarks>
/// In the product a session gives every outgoing packet straight to the core
/// (<see cref="NativeWebRtcMediaEngine.Session.SetPacketSink"/>), on the thread that produced it;
/// there is no queue to drain any more (docs/bugs/media-rode-the-shells-timer.md). Tests attach a
/// managed sink right after creating a session — attaching later would miss what the pacer
/// released in between — and read the collected packets in the old length-prefixed batch shape.
/// </remarks>
internal static class NativePacketTaps
{
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate void PacketSinkDelegate(IntPtr context, IntPtr packet, UIntPtr packetLength);

    private sealed class Tap
    {
        internal readonly ConcurrentQueue<byte[]> Packets = new();
    }

    // Kept in a static so the delegate — and the function pointer native holds — never moves or dies.
    private static readonly PacketSinkDelegate SinkDelegate = OnPacket;
    private static readonly IntPtr SinkPointer = Marshal.GetFunctionPointerForDelegate(SinkDelegate);
    private static readonly ConcurrentDictionary<long, Tap> TapsById = new();
    private static readonly ConditionalWeakTable<NativeWebRtcMediaEngine.Session, Tap> TapsBySession = new();
    private static long nextId;

    internal static void Attach(NativeWebRtcMediaEngine.Session session)
    {
        Tap tap = new();
        long id = Interlocked.Increment(ref nextId);
        TapsById[id] = tap;
        TapsBySession.AddOrUpdate(session, tap);
        session.SetPacketSink(SinkPointer, (IntPtr)id);
    }

    /// <summary>Up to <paramref name="maxPackets"/> collected packets, each prefixed by its LE16 length.</summary>
    internal static NativeWebRtcRtpPacketBatch DrainRtpPackets(
        this NativeWebRtcMediaEngine.Session session,
        uint maxPackets = 128)
    {
        if (!TapsBySession.TryGetValue(session, out Tap? tap))
        {
            throw new InvalidOperationException("session has no packet tap; call NativePacketTaps.Attach right after creating it");
        }

        List<byte[]> taken = [];
        while (taken.Count < maxPackets && tap.Packets.TryDequeue(out byte[]? packet))
        {
            taken.Add(packet);
        }

        byte[] batch = new byte[taken.Sum(packet => sizeof(ushort) + packet.Length)];
        int offset = 0;
        foreach (byte[] packet in taken)
        {
            System.Buffers.Binary.BinaryPrimitives.WriteUInt16LittleEndian(
                batch.AsSpan(offset, sizeof(ushort)),
                checked((ushort)packet.Length));
            offset += sizeof(ushort);
            packet.CopyTo(batch.AsSpan(offset));
            offset += packet.Length;
        }

        return new NativeWebRtcRtpPacketBatch(batch, batch.Length, (uint)taken.Count);
    }

    private static void OnPacket(IntPtr context, IntPtr packet, UIntPtr packetLength)
    {
        if (!TapsById.TryGetValue((long)context, out Tap? tap))
        {
            return;
        }

        byte[] copy = new byte[checked((int)packetLength)];
        Marshal.Copy(packet, copy, 0, copy.Length);
        tap.Packets.Enqueue(copy);
    }
}
