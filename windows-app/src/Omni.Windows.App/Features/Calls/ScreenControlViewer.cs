using Omni.Windows.App.Core;

namespace Omni.Windows.App.Features.Calls;

/// <summary>
/// The viewer's half of remote screen control (docs/decisions/remote-screen-control.md): collects
/// pointer, button, wheel and key input over the sharer's video and hands it to the core, which signs
/// it and sends it toward the sharer.
///
/// Sends happen off the UI thread, one at a time. Pointer moves are thinned to one per 16 ms (the
/// latest position wins); discrete events go at once. While controlling, a keep-alive goes every
/// 100 ms even with nothing new: it carries the core's redundancy window, so a lost batch is made up
/// for, and it tells the sharer we are still here (silence for a second releases our held keys).
/// Stopping lets go of everything we hold.
/// </summary>
internal sealed class ScreenControlViewer : IDisposable
{
    private const int KeepAliveMs = 100;
    private const long PointerIntervalMs = 16;

    private readonly RustBridge bridge;
    private readonly object sync = new();
    private readonly List<ScreenInputEventDto> pending = new();
    private readonly HashSet<string> heldButtons = new(StringComparer.Ordinal);
    private readonly Dictionary<(ushort Vk, ushort Scan), bool> heldKeys = new();
    private readonly Timer keepAlive;
    private (ushort X, ushort Y)? pointer;
    private bool pointerDirty;
    private long lastPointerSentMs;
    private bool sending;
    private bool stopped;
    private long sentBatches;
    private long failedBatches;
    private string lastError = string.Empty;

    internal ScreenControlViewer(RustBridge bridge, string dialogKey, string? sharer)
    {
        this.bridge = bridge;
        DialogKey = dialogKey;
        Sharer = sharer;
        keepAlive = new Timer(_ => Flush(force: true), null, KeepAliveMs, KeepAliveMs);
    }

    internal string DialogKey { get; }

    /// The sharer's member_pub in a group call; null in 1:1.
    internal string? Sharer { get; }

    internal string Diagnostics
    {
        get
        {
            lock (sync)
            {
                return $"sent={sentBatches} failed={failedBatches}" + (lastError.Length > 0 ? $" last_error={lastError}" : string.Empty);
            }
        }
    }

    /// Pointer at (`x`, `y`), each 0..1 across the shared screen.
    internal void PointerAt(double x, double y)
    {
        lock (sync)
        {
            pointer = (Normalize(x), Normalize(y));
            pointerDirty = true;
        }

        Flush(force: false);
    }

    internal void Button(string button, bool down)
    {
        lock (sync)
        {
            if (down ? !heldButtons.Add(button) : !heldButtons.Remove(button))
            {
                return; // a repeat down or an up we never sent down for
            }

            pending.Add(ScreenInputEventDto.ButtonEvent(button, down));
        }

        Flush(force: true);
    }

    internal void Wheel(short dx, short dy)
    {
        lock (sync)
        {
            pending.Add(ScreenInputEventDto.WheelEvent(dx, dy));
        }

        Flush(force: true);
    }

    internal void Key(ushort vk, ushort scan, bool extended, bool down)
    {
        lock (sync)
        {
            if (down)
            {
                heldKeys[(vk, scan)] = extended;
            }
            else if (!heldKeys.Remove((vk, scan)))
            {
                return;
            }

            pending.Add(ScreenInputEventDto.KeyEvent(vk, scan, extended, down));
        }

        Flush(force: true);
    }

    /// Stop controlling: let go of what we hold, then go quiet.
    public void Dispose()
    {
        lock (sync)
        {
            if (stopped)
            {
                return;
            }

            stopped = true;
            foreach (string button in heldButtons)
            {
                pending.Add(ScreenInputEventDto.ButtonEvent(button, down: false));
            }

            foreach (((ushort vk, ushort scan), bool extended) in heldKeys)
            {
                pending.Add(ScreenInputEventDto.KeyEvent(vk, scan, extended, down: false));
            }

            heldButtons.Clear();
            heldKeys.Clear();
        }

        keepAlive.Dispose();
        // Goes out even if a send is in flight: that send's completion flushes what is pending.
        Flush(force: true);
    }

    private void Flush(bool force)
    {
        List<ScreenInputEventDto> events;
        (ushort X, ushort Y)? position;
        lock (sync)
        {
            long now = Environment.TickCount64;
            bool pointerDue = pointerDirty && now - lastPointerSentMs >= PointerIntervalMs;
            if (sending || (!force && pending.Count == 0 && !pointerDue))
            {
                return;
            }

            sending = true;
            events = new List<ScreenInputEventDto>(pending);
            pending.Clear();
            position = pointer;
            pointerDirty = false;
            lastPointerSentMs = now;
        }

        ThreadPool.QueueUserWorkItem(_ =>
        {
            try
            {
                bridge.SendScreenControl(DialogKey, Sharer, position, events);
                lock (sync)
                {
                    sentBatches++;
                }
            }
            catch (Exception ex)
            {
                lock (sync)
                {
                    failedBatches++;
                    lastError = ex.Message.Length > 80 ? ex.Message[..80] : ex.Message;
                }
            }
            finally
            {
                bool again;
                lock (sync)
                {
                    sending = false;
                    again = pending.Count > 0 || pointerDirty;
                }

                if (again)
                {
                    Flush(force: true);
                }
            }
        });
    }

    private static ushort Normalize(double value) => (ushort)Math.Round(Math.Clamp(value, 0, 1) * 65535);
}
