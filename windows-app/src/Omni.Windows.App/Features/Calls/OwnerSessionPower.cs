using System;
using System.Runtime.InteropServices;
using System.Text;

namespace Omni.Windows.App.Features.Calls;

/// <summary>
/// The owner's machine during a remote-control session (docs/decisions/remote-access-for-contacts.md).
/// Unattended access is used exactly when nobody sits at the computer — and then the display is
/// usually asleep, and a display that is really off gives screen capture nothing to compose. (The
/// live `loop=0fps` of 2026-10-05 first blamed on this was a different bug — the frame pool raised
/// no events off the UI thread, see ScreenShareCaptureSource.Start.) So the session wakes the display and keeps it on until it ends; and it can tell when the workstation is locked,
/// which no injected input can get past (Windows keeps the secure desktop to itself).
/// </summary>
internal static class OwnerSessionPower
{
    private const uint EsContinuous = 0x80000000;
    private const uint EsSystemRequired = 0x00000001;
    private const uint EsDisplayRequired = 0x00000002;

    /// Keep the display (and the machine) on while the session runs; off releases it. Per thread —
    /// call it from the UI thread both times.
    internal static void KeepDisplayOn(bool on) =>
        SetThreadExecutionState(on ? EsContinuous | EsDisplayRequired | EsSystemRequired : EsContinuous);

    /// Wake a sleeping display now: a one-pixel injected move and back. Injected, so the
    /// "owner's own hand" guard does not mistake it for the owner.
    internal static void WakeDisplay()
    {
        Input there = new() { Type = 0, Mouse = new MouseInput { Dx = 1, Flags = 0x0001 } };
        Input back = new() { Type = 0, Mouse = new MouseInput { Dx = -1, Flags = 0x0001 } };
        SendInput(2, new[] { there, back }, Marshal.SizeOf<Input>());
    }

    /// The input desktop is not the user's ("Default"): the lock screen, the sign-in screen or a UAC
    /// prompt is up. Unknown (the call failed) counts as not locked — never invent an obstacle.
    internal static bool IsWorkstationLocked()
    {
        IntPtr desktop = OpenInputDesktop(0, false, DesktopReadObjects);
        if (desktop == IntPtr.Zero)
        {
            return true; // the secure desktop refuses us: that is the lock screen
        }

        try
        {
            var name = new StringBuilder(256);
            return GetUserObjectInformation(desktop, UoiName, name, name.Capacity * 2, out _) &&
                   !string.Equals(name.ToString(), "Default", StringComparison.OrdinalIgnoreCase);
        }
        finally
        {
            CloseDesktop(desktop);
        }
    }

    private const uint DesktopReadObjects = 0x0001;
    private const int UoiName = 2;

    [StructLayout(LayoutKind.Sequential)]
    private struct Input
    {
        public uint Type;
        public MouseInput Mouse;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct MouseInput
    {
        public int Dx;
        public int Dy;
        public uint MouseData;
        public uint Flags;
        public uint Time;
        public IntPtr ExtraInfo;
    }

    [DllImport("kernel32.dll")]
    private static extern uint SetThreadExecutionState(uint flags);

    [DllImport("user32.dll")]
    private static extern uint SendInput(uint count, Input[] inputs, int size);

    [DllImport("user32.dll", SetLastError = true)]
    private static extern IntPtr OpenInputDesktop(uint flags, bool inherit, uint access);

    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern bool GetUserObjectInformation(IntPtr obj, int index, StringBuilder info, int length, out int needed);

    [DllImport("user32.dll")]
    private static extern bool CloseDesktop(IntPtr desktop);
}
