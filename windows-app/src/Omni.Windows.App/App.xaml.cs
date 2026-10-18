using System.Runtime.InteropServices;
using System.Threading.Tasks;
using Omni.Windows.App.Core;

namespace Omni.Windows.App
{
    public partial class App : Application
    {
        private readonly string appDirectory;
        private MainWindow? window;

        public App()
        {
            InitializeComponent();
            appDirectory = ResolveAppDirectory(Environment.GetCommandLineArgs());

#if OMNI_DEV_TOOLS
            // Dev builds route the Rust core's trace log to OmniData\logs\trace.log so a normal
            // double-click launch produces the trace. Must be set BEFORE the core is created (the core
            // caches the path on its first trace call). Release builds never do this: the trace names
            // peers, addresses and timings, is fsynced per line and is not rotated — it stays off
            // unless someone sets OMNI_TRACE_FILE by hand.
            try
            {
                string logsDir = Path.Combine(appDirectory, "logs");
                Directory.CreateDirectory(logsDir);
                if (string.IsNullOrEmpty(Environment.GetEnvironmentVariable("OMNI_TRACE_FILE")))
                {
                    Environment.SetEnvironmentVariable(
                        "OMNI_TRACE_FILE",
                        Path.Combine(logsDir, "trace.log"));
                }
            }
            catch
            {
                // Never let a diagnostics-setup failure block startup.
            }
#endif

            // Without these, an unhandled UI-thread exception terminates the WinUI 3 process
            // silently with no artifact. Live-observed 2026-07-20: switching to Contacts after a
            // "forget contact" while the peer was still linked crashed with nothing on disk to
            // point at. The crash log lands inside OmniData/ so it moves with the profile folder
            // and never scatters next to the exe (portable-data-dir invariant).
            UnhandledException += OnUiThreadUnhandledException;
            AppDomain.CurrentDomain.UnhandledException += OnDomainUnhandledException;
            TaskScheduler.UnobservedTaskException += OnUnobservedTaskException;
        }

        protected override void OnLaunched(LaunchActivatedEventArgs e)
        {
            window ??= new MainWindow(appDirectory);
            // A test stand restarts Omni on a machine someone is working at (the PC, 2026-10-13): there it
            // must come up without a window in front of them. Dev/validation only; launching the exe
            // again shows the window as always (BringRunningWindowToFront).
            if (Environment.GetEnvironmentVariable("OMNI_START_HIDDEN") == "1")
            {
                AppTrace.Write("app/start", "started hidden (OMNI_START_HIDDEN)");
                window.BootWithoutWindow();
            }
            else
            {
                window.Activate();
            }
            // After the window: the native core is loaded by now, which names the extraction folder.
            BundleExtractionCleanup.RemoveStaleCopiesInBackground();
        }

        /// <summary>Another launch on this profile folder was redirected here: show the window.</summary>
        internal static void BringRunningWindowToFront()
        {
            if (Current is App { window: { } window })
            {
                window.DispatcherQueue.TryEnqueue(window.ShowFromBackground);
            }
        }

        private void OnUiThreadUnhandledException(object sender, Microsoft.UI.Xaml.UnhandledExceptionEventArgs e)
        {
            AppendCrashLog("ui_thread", e.Exception, terminating: true);
            // Let WinUI's default handling continue: the process still dies, but the log is written.
        }

        private void OnDomainUnhandledException(object sender, System.UnhandledExceptionEventArgs e)
        {
            AppendCrashLog(
                "appdomain",
                e.ExceptionObject as Exception,
                terminating: e.IsTerminating);
        }

        private void OnUnobservedTaskException(object? sender, UnobservedTaskExceptionEventArgs e)
        {
            AppendCrashLog("task_scheduler", e.Exception, terminating: false);
            // Not observed here would elevate to a process crash on some runtimes; the log has been
            // written so the follow-up crash is not silent, but we still let the runtime decide.
        }

        private void AppendCrashLog(string source, Exception? exception, bool terminating)
        {
            try
            {
                string logsDirectory = Path.Combine(appDirectory, "logs");
                Directory.CreateDirectory(logsDirectory);
                string logPath = Path.Combine(logsDirectory, "crash.log");

                List<string> lines =
                [
                    $"timestamp={DateTimeOffset.Now:O}",
                    $"source={source}",
                    $"terminating={terminating}",
                    $"process_path={Environment.ProcessPath}",
                    $"process_architecture={RuntimeInformation.ProcessArchitecture}",
                    $"exception_type={exception?.GetType().FullName ?? "<null>"}",
                    $"exception_message={exception?.Message ?? "<null>"}",
                ];

                if (exception is not null)
                {
                    lines.Add("stack_trace_begin");
                    lines.Add(exception.ToString());
                    lines.Add("stack_trace_end");
                }

                lines.Add(string.Empty);
                File.AppendAllText(logPath, string.Join(Environment.NewLine, lines) + Environment.NewLine);
            }
            catch
            {
                // The crash-writer must never itself throw — a failure here would replace one silent
                // death with another. Best-effort by design.
            }
        }

        internal static string ResolveAppDirectory(IReadOnlyList<string> args)
        {
            string? explicitAppDirectory = GetArgumentValue(args, "--app-dir");
            if (!string.IsNullOrWhiteSpace(explicitAppDirectory))
            {
                return Path.GetFullPath(explicitAppDirectory.Trim());
            }

            string? workspaceId = GetArgumentValue(args, "--workspace");
            if (!string.IsNullOrWhiteSpace(workspaceId))
            {
                return RustBridge.GetWorkspaceAppDirectory(workspaceId);
            }

            if (HasArgument(args, "--isolated"))
            {
                return RustBridge.GetWorkspaceAppDirectory($"isolated-{Guid.NewGuid():N}");
            }

            return RustBridge.GetDefaultAppDirectory();
        }

        private static bool HasArgument(IReadOnlyList<string> args, string name)
        {
            foreach (string current in args)
            {
                if (string.Equals(current, name, StringComparison.OrdinalIgnoreCase) ||
                    current.StartsWith(name + "=", StringComparison.OrdinalIgnoreCase))
                {
                    return true;
                }
            }

            return false;
        }

        private static string? GetArgumentValue(IReadOnlyList<string> args, string name)
        {
            for (int index = 0; index < args.Count; index++)
            {
                string current = args[index];
                if (string.Equals(current, name, StringComparison.OrdinalIgnoreCase) &&
                    index + 1 < args.Count)
                {
                    return args[index + 1];
                }

                if (current.StartsWith(name + "=", StringComparison.OrdinalIgnoreCase))
                {
                    return current[(name.Length + 1)..];
                }
            }

            return null;
        }
    }
}
