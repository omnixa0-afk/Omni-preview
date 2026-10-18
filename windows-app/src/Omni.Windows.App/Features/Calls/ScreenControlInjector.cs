using System.Runtime.InteropServices;
using System.Text.Json;
using Omni.Windows.App.Core;

namespace Omni.Windows.App.Features.Calls;

/// <summary>
/// The sharer's half of remote screen control (docs/decisions/remote-screen-control.md): injects the
/// input the core already verified — signed by a viewer we granted, in this call, not a replay — into
/// the shared monitor with <c>SendInput</c>.
///
/// What it adds on its own is only what the core cannot know: WHERE the shared monitor is (nothing is
/// injected unless a whole monitor is being shared) and what is held down. Held keys and buttons are
/// let go when the core says events were lost, when a viewer falls silent for a second, and when
/// control or sharing ends — a remote key must never stay stuck down on someone's computer.
///
/// Keys are injected by scan code, like RDP: the viewer presses a physical key and the sharer's own
/// layout decides the character.
///
/// The sharer's own hand wins: while they use their mouse or keyboard, remote input pauses and what
/// a viewer held is let go (<see cref="LocalInputGuard"/>, armed while control is in use).
/// </summary>
internal sealed class ScreenControlInjector : IDisposable
{
    private const long SilenceReleaseMs = 1_000;
    /// The physical-input hooks come off after this long without any remote input.
    private const long GuardIdleMs = 5_000;

    private readonly NativeSink sink;
    private readonly Func<SharedMonitor?> sharedMonitor;
    private readonly object sync = new();
    private readonly Dictionary<string, Held> held = new(StringComparer.Ordinal);
    private readonly Timer silenceTimer;
    private readonly LocalInputGuard localInput = new();
    private long lastDeliveryMs;
    private long yieldedBatches;
    private long injectedBatches;
    private long refusedBatches;
    private long releases;
    private (int X, int Y)? lastPointer;

    internal ScreenControlInjector(Func<SharedMonitor?> sharedMonitor)
    {
        this.sharedMonitor = sharedMonitor;
        sink = OnDelivery;
        silenceTimer = new Timer(_ => ReleaseSilentViewers(), null, 250, 250);
    }

    /// The function pointer the core calls (`omni_screen_control_set_sink`). Valid while this lives.
    internal IntPtr SinkPointer => Marshal.GetFunctionPointerForDelegate(sink);

    /// For the call diagnostics: batches injected / refused (no shared monitor) / releases, and where
    /// the pointer was last put.
    internal string Diagnostics
    {
        get
        {
            lock (sync)
            {
                string pointer = lastPointer is { } p ? $"{p.X},{p.Y}" : "none";
                return $"injected={injectedBatches} refused={refusedBatches} yielded={yieldedBatches} releases={releases} " +
                       $"guard={(localInput.IsArmed ? "armed" : "off")} pointer={pointer}";
            }
        }
    }

    /// The person at this computer is using it right now (their hand wins; remote input waits).
    internal bool OwnerHandActive => localInput.IsArmed && localInput.ShouldYield(Environment.TickCount64);

    /// Let go of everything every viewer holds (sharing stopped, control withdrawn, call over).
    internal void ReleaseAll()
    {
        lock (sync)
        {
            foreach (string viewer in held.Keys.ToList())
            {
                ReleaseUnsafe(viewer);
            }
        }
    }

    public void Dispose()
    {
        silenceTimer.Dispose();
        ReleaseAll();
        localInput.Dispose();
    }

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate void NativeSink(IntPtr context, IntPtr json, UIntPtr length);

    private void OnDelivery(IntPtr context, IntPtr json, UIntPtr length)
    {
        try
        {
            string text = Marshal.PtrToStringUTF8(json, checked((int)length.ToUInt64())) ?? string.Empty;
            ScreenControlDelivery? delivery =
                JsonSerializer.Deserialize(text, OmniJsonSerializerContext.Default.ScreenControlDelivery);
            if (delivery is not null)
            {
                Apply(delivery);
            }
        }
        catch (Exception)
        {
            // Called from the core's mesh pump: an exception must not cross back into Rust. A batch
            // that cannot be read is dropped; the next one carries the same events again.
        }
    }

    private void Apply(ScreenControlDelivery delivery)
    {
        lock (sync)
        {
            if (delivery.Release)
            {
                ReleaseUnsafe(delivery.Viewer);
            }

            SharedMonitor? monitor = sharedMonitor();
            if (monitor is null)
            {
                // Only a whole shared monitor is ever controlled; a window share or no share is not.
                refusedBatches++;
                ReleaseUnsafe(delivery.Viewer);
                return;
            }

            long now = Environment.TickCount64;
            lastDeliveryMs = now;
            localInput.Arm();
            if (localInput.ShouldYield(now))
            {
                // The person at this computer is using it: their hand wins, the remote one waits.
                yieldedBatches++;
                ReleaseUnsafe(delivery.Viewer);
                return;
            }

            Held state = held.TryGetValue(delivery.Viewer, out Held? existing) ? existing : held[delivery.Viewer] = new Held();
            state.LastSeenMs = Environment.TickCount64;
            if (delivery.Pointer is [ushort x, ushort y])
            {
                MovePointer(monitor, x, y);
            }

            foreach (ScreenInputEventDto input in delivery.Events)
            {
                Inject(state, input);
            }

            injectedBatches++;
        }
    }

    private void MovePointer(SharedMonitor monitor, ushort x, ushort y)
    {
        int px = monitor.Left + (int)((long)x * Math.Max(monitor.Width - 1, 0) / 65535);
        int py = monitor.Top + (int)((long)y * Math.Max(monitor.Height - 1, 0) / 65535);
        int vx = GetSystemMetrics(SmXVirtualScreen);
        int vy = GetSystemMetrics(SmYVirtualScreen);
        int vw = Math.Max(GetSystemMetrics(SmCxVirtualScreen) - 1, 1);
        int vh = Math.Max(GetSystemMetrics(SmCyVirtualScreen) - 1, 1);
        Send(Mouse(
            (int)((long)(px - vx) * 65535 / vw),
            (int)((long)(py - vy) * 65535 / vh),
            0,
            MouseMove | MouseAbsolute | MouseVirtualDesk));
        lastPointer = (px, py);
    }

    private void Inject(Held state, ScreenInputEventDto input)
    {
        switch (input.Kind)
        {
            case "button" when input.Button is { } button && input.Down is { } down:
                if (ButtonFlags(button, down) is not { } flags)
                {
                    return;
                }

                Send(Mouse(0, 0, ButtonData(button), flags));
                if (down)
                {
                    state.Buttons.Add(button);
                }
                else
                {
                    state.Buttons.Remove(button);
                }

                break;
            case "wheel":
                if (input.Dy is { } dy && dy != 0)
                {
                    Send(Mouse(0, 0, unchecked((uint)(int)dy), MouseWheel));
                }

                if (input.Dx is { } dx && dx != 0)
                {
                    Send(Mouse(0, 0, unchecked((uint)(int)dx), MouseHWheel));
                }

                break;
            case "key" when input.Vk is { } vk && input.Down is { } keyDown:
                var key = new HeldKey(vk, input.Scan ?? 0, input.Extended ?? false);
                Send(Key(key, keyDown));
                if (keyDown)
                {
                    state.Keys.Add(key);
                }
                else
                {
                    state.Keys.Remove(key);
                }

                break;
        }
    }

    private void ReleaseSilentViewers()
    {
        bool idle;
        lock (sync)
        {
            idle = Environment.TickCount64 - lastDeliveryMs > GuardIdleMs;
        }

        if (idle && localInput.IsArmed)
        {
            localInput.Disarm(); // nobody controls: the global hooks come off
        }

        lock (sync)
        {
            long now = Environment.TickCount64;
            foreach ((string viewer, Held state) in held.ToList())
            {
                if (now - state.LastSeenMs > SilenceReleaseMs && (state.Keys.Count > 0 || state.Buttons.Count > 0))
                {
                    ReleaseUnsafe(viewer);
                }
            }
        }
    }

    private void ReleaseUnsafe(string viewer)
    {
        if (!held.Remove(viewer, out Held? state) || (state.Keys.Count == 0 && state.Buttons.Count == 0))
        {
            return;
        }

        foreach (HeldKey key in state.Keys)
        {
            Send(Key(key, down: false));
        }

        foreach (string button in state.Buttons)
        {
            if (ButtonFlags(button, down: false) is { } flags)
            {
                Send(Mouse(0, 0, ButtonData(button), flags));
            }
        }

        releases++;
    }

    private sealed class Held
    {
        public long LastSeenMs = Environment.TickCount64;
        public readonly HashSet<HeldKey> Keys = new();
        public readonly HashSet<string> Buttons = new(StringComparer.Ordinal);
    }

    private readonly record struct HeldKey(ushort Vk, ushort Scan, bool Extended);

    private static uint? ButtonFlags(string button, bool down) => button switch
    {
        "left" => down ? 0x0002u : 0x0004u,
        "right" => down ? 0x0008u : 0x0010u,
        "middle" => down ? 0x0020u : 0x0040u,
        "x1" or "x2" => down ? 0x0080u : 0x0100u,
        _ => null,
    };

    private static uint ButtonData(string button) => button switch
    {
        "x1" => 1u,
        "x2" => 2u,
        _ => 0u,
    };

    private static Input Mouse(int dx, int dy, uint data, uint flags) => new()
    {
        Type = InputMouse,
        Union = new InputUnion { Mouse = new MouseInput { Dx = dx, Dy = dy, MouseData = data, Flags = flags } },
    };

    private static Input Key(HeldKey key, bool down)
    {
        uint flags = (key.Extended ? KeyExtended : 0) | (down ? 0 : KeyUp);
        // By scan code when there is one (the physical key; the sharer's layout picks the character),
        // by virtual key otherwise.
        if (key.Scan != 0)
        {
            flags |= KeyScanCode;
        }

        return new Input
        {
            Type = InputKeyboard,
            Union = new InputUnion { Keyboard = new KeybdInput { Vk = key.Scan != 0 ? (ushort)0 : key.Vk, Scan = key.Scan, Flags = flags } },
        };
    }

    private static void Send(Input input)
    {
        Input[] inputs = { input };
        _ = SendInput(1, inputs, Marshal.SizeOf<Input>());
    }

    private const uint InputMouse = 0;
    private const uint InputKeyboard = 1;
    private const uint MouseMove = 0x0001;
    private const uint MouseWheel = 0x0800;
    private const uint MouseHWheel = 0x1000;
    private const uint MouseVirtualDesk = 0x4000;
    private const uint MouseAbsolute = 0x8000;
    private const uint KeyExtended = 0x0001;
    private const uint KeyUp = 0x0002;
    private const uint KeyScanCode = 0x0008;
    private const int SmXVirtualScreen = 76;
    private const int SmYVirtualScreen = 77;
    private const int SmCxVirtualScreen = 78;
    private const int SmCyVirtualScreen = 79;

    [StructLayout(LayoutKind.Sequential)]
    private struct Input
    {
        public uint Type;
        public InputUnion Union;
    }

    [StructLayout(LayoutKind.Explicit)]
    private struct InputUnion
    {
        [FieldOffset(0)] public MouseInput Mouse;
        [FieldOffset(0)] public KeybdInput Keyboard;
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

    [StructLayout(LayoutKind.Sequential)]
    private struct KeybdInput
    {
        public ushort Vk;
        public ushort Scan;
        public uint Flags;
        public uint Time;
        public IntPtr ExtraInfo;
    }

    [DllImport("user32.dll", SetLastError = true)]
    private static extern uint SendInput(uint count, Input[] inputs, int size);

    [DllImport("user32.dll")]
    private static extern int GetSystemMetrics(int index);
}
