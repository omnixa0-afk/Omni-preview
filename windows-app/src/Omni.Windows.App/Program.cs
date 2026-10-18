using System.Diagnostics;
using System.Runtime.InteropServices;

namespace Omni.Windows.App
{
    // Custom entry point, replacing the XAML-generated Main (disabled via DISABLE_XAML_GENERATED_MAIN).
    //
    // WinUI 3 UNPACKAGED resolves Window/Page XAML (`ms-appx:///MainWindow.xaml`) through MRT, whose
    // root resource-map name is derived from the EXECUTABLE's base name. So a renamed exe — e.g. the
    // "Omni.Windows.App (1).exe" a browser/copy produces — can't find MainWindow.xaml and dies with
    // `XamlParseException: XAML parsing failed` on the first InitializeComponent (confirmed live
    // 2026-07-25). If we were launched under a non-canonical name, re-launch a canonically-named
    // hardlink of ourselves (no byte copy) so the app just works regardless of the file name.
    // See docs/bugs/renamed-exe-xamlparse.md.
    public static class Program
    {
        private const string CanonicalName = "Omni.Windows.App";

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool CreateHardLink(
            string lpFileName,
            string lpExistingFileName,
            nint lpSecurityAttributes);

        [System.STAThread]
        private static void Main(string[] args)
        {
            if (TryRelaunchUnderCanonicalName(args))
            {
                return; // a canonically-named instance was started; this launcher exits.
            }
            global::WinRT.ComWrappersSupport.InitializeComWrappers();
            if (RedirectToRunningInstance())
            {
                return; // Omni already runs on this profile folder and was brought to the front.
            }
            StartApp();
        }

        // One Omni per profile folder (docs/decisions/multi-device-equal-nodes.md, step 1.5): two
        // processes on one database would be two nodes with one identity. A second launch hands its
        // activation to the running instance — which shows its window, from the tray too — and exits.
        // The core enforces the same with a file lock; this is the friendly half of it.
        private static bool RedirectToRunningInstance()
        {
            try
            {
                string key = "omni-profile:" + App.ResolveAppDirectory(Environment.GetCommandLineArgs())
                    .ToLowerInvariant();
                var keyInstance = Microsoft.Windows.AppLifecycle.AppInstance.FindOrRegisterForKey(key);
                if (keyInstance.IsCurrent)
                {
                    keyInstance.Activated += (_, _) => App.BringRunningWindowToFront();
                    return false;
                }

                var activation = Microsoft.Windows.AppLifecycle.AppInstance.GetCurrent().GetActivatedEventArgs();
                // Off the STA thread: the redirect is a cross-process COM call.
                System.Threading.Tasks.Task.Run(() => keyInstance.RedirectActivationToAsync(activation).AsTask().Wait())
                    .Wait();
                return true;
            }
            catch (Exception exception)
            {
                // Never block startup on this: the core's profile lock still refuses a second node.
                Debug.WriteLine($"single-instance redirect failed: {exception.Message}");
                return false;
            }
        }

        // The exact WinUI-generated startup (kept verbatim from App.g.i.cs so behaviour is identical).
        private static void StartApp()
        {
            global::Microsoft.UI.Xaml.Application.Start((p) =>
            {
                var context = new global::Microsoft.UI.Dispatching.DispatcherQueueSynchronizationContext(
                    global::Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread());
                global::System.Threading.SynchronizationContext.SetSynchronizationContext(context);
                _ = new App();
            });
        }

        // Returns true if a canonically-named process was started (this one should exit); false to run
        // in-place. Best-effort: any failure (read-only folder, cross-volume, blocked) falls through to
        // running in-place — no worse than before, and the crash log still lands in OmniData\logs.
        private static bool TryRelaunchUnderCanonicalName(string[] args)
        {
            try
            {
                string? exePath = Environment.ProcessPath;
                if (string.IsNullOrEmpty(exePath)
                    || string.Equals(
                        Path.GetFileNameWithoutExtension(exePath),
                        CanonicalName,
                        StringComparison.OrdinalIgnoreCase))
                {
                    return false; // already canonical (or unknown) — run normally.
                }

                string? dir = Path.GetDirectoryName(exePath);
                if (string.IsNullOrEmpty(dir))
                {
                    return false;
                }

                string canonical = Path.Combine(dir, CanonicalName + ".exe");
                if (!File.Exists(canonical))
                {
                    // Prefer a hardlink (same bytes, same volume, no duplication); fall back to a copy.
                    if (!CreateHardLink(canonical, exePath, 0))
                    {
                        File.Copy(exePath, canonical);
                    }
                }

                var psi = new ProcessStartInfo
                {
                    FileName = canonical,
                    UseShellExecute = false,
                    WorkingDirectory = dir,
                };
                foreach (string arg in args)
                {
                    psi.ArgumentList.Add(arg);
                }
                Process.Start(psi);
                return true;
            }
            catch
            {
                return false;
            }
        }
    }
}
