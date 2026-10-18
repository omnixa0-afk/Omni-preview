using System.Globalization;
using System.Runtime.InteropServices;
using System.Text;

namespace Omni.Windows.App;

/// <summary>
/// UI-test-channel commands for remote screen control (docs/decisions/remote-screen-control.md). The
/// grant and the toggle go through the same handlers as the buttons; input goes into the same viewer
/// the pointer surface feeds. `sc.state` reports, on the sharer, what was injected and where the
/// real cursor is — the proof that SendInput landed, without anyone looking at the screen.
/// </summary>
public sealed partial class MainWindow
{
    private string? TryExecuteScreenControlUiTestCommand(string command, IReadOnlyList<string> args)
    {
        switch (command)
        {
            case "sc.grant":
                return Answer(SetCallScreenControlGrant(args.Count > 0 && args[0] == "on"));

            case "sc.control":
                if (args.Count > 0 && args[0] == "on")
                {
                    RefreshCallScreenControlState();
                    if (currentChatCallSummary is not { } call || !callScreenControlState.PeerGrantsMe)
                    {
                        return "error: the peer has not granted control";
                    }

                    StartRemoteScreenControl(call.DialogKey, sharer: null, RemoteScreenControlSurface);
                }
                else
                {
                    StopRemoteScreenControl();
                }

                return Answer("control " + (screenControlViewer is null ? "off" : "on"));

            case "group.sc.grant":
                // group.sc.grant <group_id> all|none|<member_pub>[,<member_pub>...]
                if (args.Count < 2)
                {
                    return "error: usage group.sc.grant <group_id> all|none|<member_pub,...>";
                }

                InstallScreenControlSink();
                bool everyone = args[1] == "all";
                IReadOnlyList<string> members = everyone || args[1] == "none"
                    ? Array.Empty<string>()
                    : args[1].Split(',', StringSplitOptions.RemoveEmptyEntries);
                rustBridge.SetGroupCallScreenControl(args[0], everyone, members);
                return Answer("granted " + args[1]);

            case "group.sc.control":
                // group.sc.control <group_id> <sharer_member_pub> on|off
                if (args.Count < 3)
                {
                    return "error: usage group.sc.control <group_id> <sharer_member_pub> on|off";
                }

                if (args[2] == "on")
                {
                    StartRemoteScreenControl("group:" + args[0], args[1], RemoteScreenControlSurface);
                }
                else
                {
                    StopRemoteScreenControl();
                }

                return Answer("control " + (screenControlViewer is null ? "off" : "on"));

            case "sc.move":
                // sc.move <x 0..1> <y 0..1>
                if (screenControlViewer is not { } mover || args.Count < 2)
                {
                    return "error: not controlling (sc.control on) or usage sc.move <x> <y>";
                }

                mover.PointerAt(
                    double.Parse(args[0], CultureInfo.InvariantCulture),
                    double.Parse(args[1], CultureInfo.InvariantCulture));
                return Answer("moved");

            // A real OS click at a point of this window (DIPs, as ui.shot shows them): what the
            // user's own mouse does, through hit-testing and pointer capture, unlike a peer invoke.
            case "ui.click":
            {
                if (args.Count < 2 || Content is not UIElement { XamlRoot: { } xamlRoot })
                {
                    return "error: ui.click <x> <y> [right]";
                }

                double scale = xamlRoot.RasterizationScale;
                IntPtr hwnd = WinRT.Interop.WindowNative.GetWindowHandle(this);
                CursorPoint point = new()
                {
                    X = (int)Math.Round(double.Parse(args[0], CultureInfo.InvariantCulture) * scale),
                    Y = (int)Math.Round(double.Parse(args[1], CultureInfo.InvariantCulture) * scale),
                };
                ClientToScreen(hwnd, ref point);
                SetForegroundWindow(hwnd);
                SetCursorPos(point.X, point.Y);
                bool right = args.Count > 2 && args[2] == "right";
                mouse_event(right ? 0x0008u : 0x0002u, 0, 0, 0, UIntPtr.Zero);
                mouse_event(right ? 0x0010u : 0x0004u, 0, 0, 0, UIntPtr.Zero);
                return $"ok screen={point.X},{point.Y}";
            }

            case "sc.click":
                if (screenControlViewer is not { } clicker)
                {
                    return "error: not controlling";
                }

                string button = args.Count > 0 ? args[0] : "left";
                clicker.Button(button, down: true);
                clicker.Button(button, down: false);
                return Answer("clicked " + button);

            case "sc.key":
                // sc.key <vk> <scan> [down|up|tap]
                if (screenControlViewer is not { } typist || args.Count < 2)
                {
                    return "error: not controlling or usage sc.key <vk> <scan> [down|up|tap]";
                }

                ushort vk = ushort.Parse(args[0], CultureInfo.InvariantCulture);
                ushort scan = ushort.Parse(args[1], CultureInfo.InvariantCulture);
                string how = args.Count > 2 ? args[2] : "tap";
                if (how is "down" or "tap")
                {
                    typist.Key(vk, scan, extended: false, down: true);
                }

                if (how is "up" or "tap")
                {
                    typist.Key(vk, scan, extended: false, down: false);
                }

                return Answer("key " + how);

            case "sc.state":
                return BuildScreenControlUiTestState();

            default:
                return null;
        }

        string Answer(string what) => "ok\n" + what + "\n" + BuildScreenControlUiTestState()["ok\n".Length..];
    }

    private string BuildScreenControlUiTestState()
    {
        RefreshCallScreenControlState();
        StringBuilder builder = new("ok");
        builder.AppendLine();
        builder.AppendLine($"i_grant={callScreenControlState.IGrant.ToString().ToLowerInvariant()}");
        builder.AppendLine($"peer_grants_me={callScreenControlState.PeerGrantsMe.ToString().ToLowerInvariant()}");
        builder.AppendLine(callVideoService.SharedMonitor is { } monitor
            ? $"shared_monitor={monitor.Left},{monitor.Top},{monitor.Width}x{monitor.Height}"
            : "shared_monitor=none");
        builder.AppendLine($"injector={screenControlInjector?.Diagnostics ?? "none"}");
        builder.AppendLine($"viewer={screenControlViewer?.Diagnostics ?? "none"}");
        builder.AppendLine(callVideoService.RemotePresentedSize is { } picture
            ? $"presented={picture.Width}x{picture.Height}"
            : "presented=none");
        builder.AppendLine(GetCursorPos(out CursorPoint cursor) ? $"cursor={cursor.X},{cursor.Y}" : "cursor=unknown");
        return builder.ToString().TrimEnd();
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct CursorPoint
    {
        public int X;
        public int Y;
    }

    [DllImport("user32.dll")]
    private static extern bool GetCursorPos(out CursorPoint point);

    [DllImport("user32.dll")]
    private static extern bool ClientToScreen(IntPtr hwnd, ref CursorPoint point);

    [DllImport("user32.dll")]
    private static extern bool SetCursorPos(int x, int y);

    [DllImport("user32.dll")]
    private static extern bool SetForegroundWindow(IntPtr hwnd);

    [DllImport("user32.dll")]
    private static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extraInfo);
}
