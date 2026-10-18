using System;
using System.Linq;
using System.Runtime.CompilerServices;
using System.Text.Json;

namespace Omni.Windows.App.Core;

/// Multi-call entry point: this exe is BOTH the app and its courier helper processes.
///
/// The rendezvous (ntfy/Nostr/BT-DHT) and Mainline-DHT couriers must run as separate PROCESSES —
/// that is the isolation they exist for: key-free, untrusted, independently proxyable, and a crash
/// there can't take the app down (see the helper crates' manifests). In the normal multi-file build
/// the core spawns the sibling <c>omni-*-helper.exe</c>. A single-file build has no siblings, and
/// Windows cannot start a process from inside the bundle — so the core re-execs THIS exe with
/// <c>--run-helper &lt;kind&gt; …</c>, and this intercepts that before any UI exists and hands the
/// args to the same courier code, now linked into <c>omni_ffi.dll</c>. Nothing is extracted to disk:
/// one file stays one file, and it just runs as several processes.
///
/// Runs from a <c>[ModuleInitializer]</c>, i.e. before <c>Main</c>, so a helper process never brings
/// up WinUI/XAML at all. It also publishes <c>OMNI_HELPER_SELF_EXEC</c> for the core's spawn-side
/// gate — the pair is one contract: only a host that honours the flag may be re-exec'd as a helper.
internal static class HelperMode
{
    private const string Flag = "--run-helper";

    private const string SelfExecEnv = "OMNI_HELPER_SELF_EXEC";

    /// True when this process can launch a courier via self-exec (<c>--run-helper &lt;kind&gt;</c>),
    /// i.e. a helper starts even with no sibling <c>omni-*-helper.exe</c> on disk — the single-file
    /// case. The module initializer always sets <see cref="SelfExecEnv"/> and intercepts the flag, so
    /// this holds for every WinUI launch (folder or single-file). It reads the SAME env var the Rust
    /// spawn side gates on (<c>app_rendezvous.rs::helper_launch</c>), so the two never disagree.
    /// The UI helper-availability check consults this so a single-file build does not falsely tell the
    /// user to "rebuild the app" when self-exec would launch the DHT leg fine.
    internal static bool SelfExecSupported =>
        string.Equals(Environment.GetEnvironmentVariable(SelfExecEnv), "1", StringComparison.Ordinal);

    /// Exit code for "we were asked to be a helper and couldn't be" — distinct from any code the
    /// helper itself returns (0 ok, 1 bind/engine failure, 2 usage), so a spawn failure is legible
    /// in the parent's stderr capture rather than looking like a normal helper exit.
    private const int HelperHostFailureExitCode = 70;

    [ModuleInitializer]
    internal static void Initialize()
    {
        // The core only self-execs when this says the host serves `--run-helper`; the sibling
        // binaries still win where they exist, so the dev multi-file build is unaffected.
        Environment.SetEnvironmentVariable(SelfExecEnv, "1");

        string[] args = Environment.GetCommandLineArgs();
        int flagIndex = Array.FindIndex(
            args,
            arg => string.Equals(arg, Flag, StringComparison.OrdinalIgnoreCase));
        if (flagIndex < 0)
        {
            return; // Normal app launch.
        }

        Environment.Exit(RunHelper(args, flagIndex));
    }

    private static int RunHelper(string[] args, int flagIndex)
    {
        try
        {
            if (flagIndex + 1 >= args.Length)
            {
                Console.Error.WriteLine($"{Flag} needs a helper kind (rendezvous|dht)");
                return HelperHostFailureExitCode;
            }

            string kind = args[flagIndex + 1];
            string[] helperArgs = args.Skip(flagIndex + 2).ToArray();
            string argsJson = JsonSerializer.Serialize(
                helperArgs,
                OmniJsonSerializerContext.Default.StringArray);
            return NativeMethods.omni_run_helper(kind, argsJson);
        }
        catch (Exception ex)
        {
            // The parent reads our stderr to learn the helper's port, so a failure must show up
            // there — it is the only channel this process has.
            Console.Error.WriteLine($"cannot run helper: {ex.GetType().Name}: {ex.Message}");
            return HelperHostFailureExitCode;
        }
    }
}
