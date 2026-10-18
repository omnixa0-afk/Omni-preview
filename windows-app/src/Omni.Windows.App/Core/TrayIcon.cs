using System.Runtime.InteropServices;

namespace Omni.Windows.App.Core;

/// <summary>
/// The notification-area icon Omni shows while it runs in the background (window closed, app alive),
/// so a contact with standing remote access can still reach this computer
/// (docs/decisions/remote-access-for-contacts.md, stage 2). Plain Win32 (Shell_NotifyIcon) on a hidden
/// window of our own: no package, nothing installed, works from the single-file exe.
///
/// Created and used on the UI thread — its window procedure runs on that thread's message loop.
/// </summary>
internal sealed class TrayIcon : IDisposable
{
    private const string WindowClassName = "OmniTrayIconWindow";
    private const string IconResourceName = "Omni.Windows.App.TrayIcon.png";
    private const uint IconId = 1;
    private const uint WmApp = 0x8000;
    private const uint CallbackMessage = WmApp + 1;
    private const uint WmLButtonUp = 0x0202;
    private const uint WmRButtonUp = 0x0205;
    private const uint NimAdd = 0, NimDelete = 2;
    private const uint NifMessage = 0x1, NifIcon = 0x2, NifTip = 0x4;
    private const uint MfString = 0x0, MfSeparator = 0x800;
    private const uint TpmReturnCmd = 0x100, TpmRightButton = 0x2;
    private const uint MenuOpen = 1, MenuExit = 2;

    private readonly WndProc wndProc; // held: the native side keeps only a pointer to it
    private readonly nint window;
    private readonly nint icon;
    private readonly bool ownsIcon;
    private readonly uint taskbarCreatedMessage;
    private readonly string tooltip;
    private bool shown;
    private bool disposed;

    /// <summary>Left click, or "Открыть Omni" in the menu.</summary>
    public event Action? OpenRequested;

    /// <summary>"Выход" in the menu: the user wants the app gone, background or not.</summary>
    public event Action? ExitRequested;

    public TrayIcon(string tooltip)
    {
        this.tooltip = tooltip.Length > 127 ? tooltip[..127] : tooltip;
        wndProc = OnWindowMessage;
        nint instance = GetModuleHandleW(null);
        var windowClass = new WndClassEx
        {
            Size = (uint)Marshal.SizeOf<WndClassEx>(),
            WndProc = Marshal.GetFunctionPointerForDelegate(wndProc),
            Instance = instance,
            ClassName = WindowClassName,
        };
        RegisterClassExW(ref windowClass); // a second instance finds it registered; harmless
        // A hidden top-level window rather than a message-only one: only top-level windows hear
        // "TaskbarCreated", which is how the icon comes back after Explorer restarts.
        window = CreateWindowExW(0, WindowClassName, "Omni", 0, 0, 0, 0, 0, 0, 0, instance, 0);
        taskbarCreatedMessage = RegisterWindowMessageW("TaskbarCreated");
        (icon, ownsIcon) = LoadTrayIconImage();
    }

    public void Show()
    {
        if (disposed || shown || window == 0)
        {
            return;
        }

        var data = NewNotifyIconData();
        data.Flags = NifMessage | NifIcon | NifTip;
        data.CallbackMessage = CallbackMessage;
        data.Icon = icon;
        data.Tip = tooltip;
        shown = Shell_NotifyIconW(NimAdd, ref data);
        if (!shown)
        {
            AppTrace.Write("app/tray", "Shell_NotifyIcon(NIM_ADD) failed");
        }
    }

    public void Hide()
    {
        if (!shown)
        {
            return;
        }

        var data = NewNotifyIconData();
        Shell_NotifyIconW(NimDelete, ref data);
        shown = false;
    }

    public void Dispose()
    {
        if (disposed)
        {
            return;
        }

        Hide();
        disposed = true;
        if (window != 0)
        {
            DestroyWindow(window);
        }

        if (ownsIcon && icon != 0)
        {
            DestroyIcon(icon);
        }
    }

    private nint OnWindowMessage(nint hwnd, uint message, nint wParam, nint lParam)
    {
        if (message == CallbackMessage && wParam == (nint)IconId)
        {
            switch ((uint)(lParam & 0xFFFF))
            {
                case WmLButtonUp:
                    OpenRequested?.Invoke();
                    break;
                case WmRButtonUp:
                    ShowMenu();
                    break;
            }

            return 0;
        }

        if (message == taskbarCreatedMessage && shown)
        {
            shown = false; // Explorer restarted and forgot the icon; put it back
            Show();
            return 0;
        }

        return DefWindowProcW(hwnd, message, wParam, lParam);
    }

    private void ShowMenu()
    {
        nint menu = CreatePopupMenu();
        if (menu == 0)
        {
            return;
        }

        try
        {
            AppendMenuW(menu, MfString, MenuOpen, "Открыть Omni");
            AppendMenuW(menu, MfSeparator, 0, null);
            AppendMenuW(menu, MfString, MenuExit, "Выход");
            GetCursorPos(out Point cursor);
            // Without this the menu does not close when the user clicks elsewhere (documented
            // TrackPopupMenu behaviour for notification-area menus).
            SetForegroundWindow(window);
            uint command = TrackPopupMenu(menu, TpmReturnCmd | TpmRightButton, cursor.X, cursor.Y, 0, window, 0);
            if (command == MenuOpen)
            {
                OpenRequested?.Invoke();
            }
            else if (command == MenuExit)
            {
                ExitRequested?.Invoke();
            }
        }
        finally
        {
            DestroyMenu(menu);
        }
    }

    private NotifyIconData NewNotifyIconData() => new()
    {
        Size = (uint)Marshal.SizeOf<NotifyIconData>(),
        Window = window,
        Id = IconId,
        Tip = string.Empty,
        Info = string.Empty,
        InfoTitle = string.Empty,
    };

    // The app logo is embedded in the assembly (one file, nothing next to the exe). Windows builds an
    // icon straight from PNG bytes; if that ever fails, the stock application icon still works.
    private static (nint Icon, bool Owned) LoadTrayIconImage()
    {
        try
        {
            using Stream? stream = typeof(TrayIcon).Assembly.GetManifestResourceStream(IconResourceName);
            if (stream is not null)
            {
                byte[] png = new byte[stream.Length];
                stream.ReadExactly(png);
                nint created = CreateIconFromResourceEx(png, (uint)png.Length, true, 0x00030000, 0, 0, 0);
                if (created != 0)
                {
                    return (created, true);
                }
            }
        }
        catch (Exception ex)
        {
            AppTrace.WriteFailure("app/tray", "load icon", ex);
        }

        return (LoadIconW(0, IdiApplication), false);
    }

    private const int IdiApplication = 32512;

    private delegate nint WndProc(nint hwnd, uint message, nint wParam, nint lParam);

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct WndClassEx
    {
        public uint Size;
        public uint Style;
        public nint WndProc;
        public int ClassExtra;
        public int WindowExtra;
        public nint Instance;
        public nint Icon;
        public nint Cursor;
        public nint Background;
        public string? MenuName;
        public string ClassName;
        public nint IconSmall;
    }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct NotifyIconData
    {
        public uint Size;
        public nint Window;
        public uint Id;
        public uint Flags;
        public uint CallbackMessage;
        public nint Icon;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)]
        public string Tip;
        public uint State;
        public uint StateMask;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 256)]
        public string Info;
        public uint TimeoutOrVersion;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 64)]
        public string InfoTitle;
        public uint InfoFlags;
        public Guid Item;
        public nint BalloonIcon;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct Point
    {
        public int X;
        public int Y;
    }

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode)]
    private static extern nint GetModuleHandleW(string? moduleName);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern ushort RegisterClassExW(ref WndClassEx windowClass);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern nint CreateWindowExW(
        uint exStyle, string className, string windowName, uint style,
        int x, int y, int width, int height, nint parent, nint menu, nint instance, nint param);

    [DllImport("user32.dll")]
    private static extern bool DestroyWindow(nint window);

    [DllImport("user32.dll")]
    private static extern nint DefWindowProcW(nint window, uint message, nint wParam, nint lParam);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern uint RegisterWindowMessageW(string name);

    [DllImport("shell32.dll", CharSet = CharSet.Unicode)]
    private static extern bool Shell_NotifyIconW(uint message, ref NotifyIconData data);

    [DllImport("user32.dll")]
    private static extern nint CreatePopupMenu();

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern bool AppendMenuW(nint menu, uint flags, nuint id, string? text);

    [DllImport("user32.dll")]
    private static extern bool DestroyMenu(nint menu);

    [DllImport("user32.dll")]
    private static extern uint TrackPopupMenu(nint menu, uint flags, int x, int y, int reserved, nint window, nint rect);

    [DllImport("user32.dll")]
    private static extern bool GetCursorPos(out Point point);

    [DllImport("user32.dll")]
    private static extern bool SetForegroundWindow(nint window);

    [DllImport("user32.dll")]
    private static extern nint CreateIconFromResourceEx(
        byte[] data, uint size, bool isIcon, uint version, int width, int height, uint flags);

    [DllImport("user32.dll")]
    private static extern nint LoadIconW(nint instance, nint iconName);

    [DllImport("user32.dll")]
    private static extern bool DestroyIcon(nint icon);
}
