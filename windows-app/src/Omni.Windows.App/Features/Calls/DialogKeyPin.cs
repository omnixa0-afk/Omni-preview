using System.Runtime.InteropServices;
using System.Text;

namespace Omni.Windows.App.Features.Calls;

/// <summary>
/// Pinned UTF-8 buffer for a dialog key. Created once at call start and
/// passed by pointer to every FFI media call (send/drain datagrams),
/// avoiding ~60 string→UTF-8 marshallings per second per active dialog
/// (30fps × send+drain = 60 calls).
/// </summary>
/// <remarks>
/// Wave 2 (2026-05-16). Backing byte[] is pinned via <see cref="GCHandle"/>
/// so the address stays valid across managed GC. The handle is released in
/// <see cref="Dispose"/> — caller MUST dispose at call end or sign-out,
/// otherwise the buffer never moves and we leak managed memory.
/// </remarks>
internal sealed class DialogKeyPin : IDisposable
{
    private readonly byte[] buffer;
    private GCHandle pin;
    private bool disposed;

    internal DialogKeyPin(string dialogKey)
    {
        if (string.IsNullOrEmpty(dialogKey))
        {
            throw new ArgumentException("dialogKey must not be empty.", nameof(dialogKey));
        }

        buffer = Encoding.UTF8.GetBytes(dialogKey);
        pin = GCHandle.Alloc(buffer, GCHandleType.Pinned);
        Pointer = pin.AddrOfPinnedObject();
        Length = (UIntPtr)buffer.Length;
        DialogKey = dialogKey;
    }

    internal IntPtr Pointer { get; }

    internal UIntPtr Length { get; }

    internal string DialogKey { get; }

    public void Dispose()
    {
        if (disposed)
        {
            return;
        }

        disposed = true;
        if (pin.IsAllocated)
        {
            pin.Free();
        }
    }
}
