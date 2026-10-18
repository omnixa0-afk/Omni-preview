using System.Runtime.InteropServices;

namespace Omni.Windows.App.Features.Calls;

/// <summary>
/// The sharer's own hand wins over a remote one (docs/decisions/remote-screen-control.md, «Приоритет
/// своего ввода»): while the person at the computer moves their mouse or types, remote input pauses,
/// and resumes a moment after they stop — the way TeamViewer and AnyDesk behave.
///
/// Physical input is told from injected input by the standard flag low-level hooks carry
/// (<c>LLMHF_INJECTED</c> / <c>LLKHF_INJECTED</c>): everything <c>SendInput</c> puts in — ours, from any
/// viewer — is flagged, a real mouse and keyboard are not. The hooks run on their own thread with a
/// message loop, installed only while remote control is in use (<see cref="Arm"/>) and removed after
/// it goes quiet (<see cref="Disarm"/>): a global hook touches every input event of the system, so it
/// must not sit there when nobody is controlling.
/// </summary>
internal sealed class LocalInputGuard : IDisposable
{
    /// How long remote input stays paused after the last physical input.
    internal const long YieldMs = 1_000;

    private readonly object sync = new();
    private readonly HookProc mouseProc;
    private readonly HookProc keyboardProc;
    private Thread? hookThread;
    private uint hookThreadId;
    private long lastLocalInputMs = long.MinValue / 2;
    private long eventsSeen;

    /// Every input event the hooks saw, physical or injected (diagnostics; proves the hooks are live).
    internal long EventsSeen => Interlocked.Read(ref eventsSeen);

    /// Win32 error of the last failed hook install; 0 when both hooks are in.
    internal int InstallError { get; private set; }

    internal LocalInputGuard()
    {
        mouseProc = (code, message, info) => OnHook(code, message, info, MouseInjectedFlag);
        keyboardProc = (code, message, info) => OnHook(code, message, info, KeyboardInjectedFlag);
    }

    /// Physical input happened at `nowMs` (the hooks call this; also the seam for tests).
    internal void NoteLocalInput(long nowMs) => Interlocked.Exchange(ref lastLocalInputMs, nowMs);

    /// Whether remote input must wait at `nowMs`: the person at the computer touched it within
    /// <see cref="YieldMs"/>.
    internal bool ShouldYield(long nowMs) => nowMs - Interlocked.Read(ref lastLocalInputMs) < YieldMs;

    internal bool IsArmed
    {
        get
        {
            lock (sync)
            {
                return hookThread is not null;
            }
        }
    }

    /// Install the hooks (idempotent).
    internal void Arm()
    {
        lock (sync)
        {
            if (hookThread is not null)
            {
                return;
            }

            using ManualResetEventSlim started = new();
            hookThread = new Thread(() => RunHooks(started)) { IsBackground = true, Name = "omni-local-input-guard" };
            hookThread.Start();
            started.Wait(TimeSpan.FromSeconds(2));
        }
    }

    /// Remove the hooks (idempotent).
    internal void Disarm()
    {
        Thread? thread;
        lock (sync)
        {
            thread = hookThread;
            hookThread = null;
            if (thread is not null && hookThreadId != 0)
            {
                _ = PostThreadMessage(hookThreadId, WmQuit, IntPtr.Zero, IntPtr.Zero);
            }
        }

        thread?.Join(TimeSpan.FromSeconds(2));
    }

    public void Dispose() => Disarm();

    private void RunHooks(ManualResetEventSlim started)
    {
        hookThreadId = GetCurrentThreadId();
        IntPtr module = GetModuleHandle(null);
        IntPtr mouseHook = SetWindowsHookEx(WhMouseLl, mouseProc, module, 0);
        IntPtr keyboardHook = SetWindowsHookEx(WhKeyboardLl, keyboardProc, module, 0);
        InstallError = mouseHook == IntPtr.Zero || keyboardHook == IntPtr.Zero ? Marshal.GetLastWin32Error() : 0;
        started.Set();
        try
        {
            // Low-level hooks are called through this thread's message queue: it must pump.
            while (GetMessage(out Msg message, IntPtr.Zero, 0, 0) > 0)
            {
                _ = TranslateMessage(ref message);
                _ = DispatchMessage(ref message);
            }
        }
        finally
        {
            if (mouseHook != IntPtr.Zero)
            {
                _ = UnhookWindowsHookEx(mouseHook);
            }

            if (keyboardHook != IntPtr.Zero)
            {
                _ = UnhookWindowsHookEx(keyboardHook);
            }

            hookThreadId = 0;
        }
    }

    private IntPtr OnHook(int code, IntPtr message, IntPtr info, uint injectedFlag)
    {
        // `flags` sits at a different offset in the two structs (MSLLHOOKSTRUCT 12, KBDLLHOOKSTRUCT 8).
        if (code >= 0 && info != IntPtr.Zero)
        {
            Interlocked.Increment(ref eventsSeen);
            int offset = injectedFlag == MouseInjectedFlag ? MouseFlagsOffset : KeyboardFlagsOffset;
            uint flags = (uint)Marshal.ReadInt32(info, offset);
            if ((flags & injectedFlag) == 0)
            {
                NoteLocalInput(Environment.TickCount64);
            }
        }

        return CallNextHookEx(IntPtr.Zero, code, message, info);
    }

    private const int WhKeyboardLl = 13;
    private const int WhMouseLl = 14;
    private const uint WmQuit = 0x0012;
    private const uint MouseInjectedFlag = 0x01; // LLMHF_INJECTED
    private const uint KeyboardInjectedFlag = 0x10; // LLKHF_INJECTED
    private const int MouseFlagsOffset = 12; // MSLLHOOKSTRUCT: POINT pt (8), mouseData (4), flags
    private const int KeyboardFlagsOffset = 8; // KBDLLHOOKSTRUCT: vkCode (4), scanCode (4), flags

    private delegate IntPtr HookProc(int code, IntPtr message, IntPtr info);

    [StructLayout(LayoutKind.Sequential)]
    private struct Msg
    {
        public IntPtr Hwnd;
        public uint Message;
        public IntPtr WParam;
        public IntPtr LParam;
        public uint Time;
        public int X;
        public int Y;
    }

    [DllImport("user32.dll", SetLastError = true)]
    private static extern IntPtr SetWindowsHookEx(int hook, HookProc proc, IntPtr module, uint threadId);

    [DllImport("user32.dll")]
    private static extern bool UnhookWindowsHookEx(IntPtr hook);

    [DllImport("user32.dll")]
    private static extern IntPtr CallNextHookEx(IntPtr hook, int code, IntPtr message, IntPtr info);

    [DllImport("user32.dll")]
    private static extern int GetMessage(out Msg message, IntPtr hwnd, uint min, uint max);

    [DllImport("user32.dll")]
    private static extern bool TranslateMessage(ref Msg message);

    [DllImport("user32.dll")]
    private static extern IntPtr DispatchMessage(ref Msg message);

    [DllImport("user32.dll")]
    private static extern bool PostThreadMessage(uint threadId, uint message, IntPtr wParam, IntPtr lParam);

    [DllImport("kernel32.dll")]
    private static extern uint GetCurrentThreadId();

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode)]
    private static extern IntPtr GetModuleHandle(string? name);
}
