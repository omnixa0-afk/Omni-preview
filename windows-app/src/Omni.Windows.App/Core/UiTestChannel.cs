using Microsoft.UI.Dispatching;

namespace Omni.Windows.App.Core;

/// <summary>
/// A file-drop command channel for driving the app in an automated live test.
///
/// Two-machine call testing is the only way to find the bugs that actually bite (a hot
/// mic, a call that ends on one side, a share that freezes) and it was entirely manual:
/// a human on each machine, clicking, and the only record afterwards was a trace and a
/// memory of what was pressed. This lets a script drive both ends and, more importantly,
/// ASSERT on state instead of on a screenshot.
///
/// Protocol, deliberately the dumbest thing that works over `ssh` and `Set-Content`:
///   * write a UTF-8 file `&lt;dir&gt;\&lt;name&gt;.cmd` holding one line: `command [args…]`
///   * the app executes it and writes `&lt;dir&gt;\&lt;name&gt;.out`, first line `ok` or `error: …`
///   * the `.cmd` is deleted as soon as it is read, so nothing runs twice
/// No sockets, no ports, no JSON quoting through two shells.
///
/// SECURITY. This is a messenger, so the channel is off unless `OMNI_UI_TEST_CHANNEL` is
/// set to an absolute directory path, and it exposes ONLY call and media control plus a
/// state dump — it cannot read or send messages, touch contacts, or export anything. It
/// is a development tool; see docs/decisions/ui-test-channel.md.
/// </summary>
internal sealed class UiTestChannel : IAsyncDisposable
{
    internal const string EnableEnvironmentVariable = "OMNI_UI_TEST_CHANNEL";
    private const string RequestExtension = ".cmd";
    private const string ResponseExtension = ".out";
    private static readonly TimeSpan PollInterval = TimeSpan.FromMilliseconds(200);

    private readonly string directory;
    private readonly DispatcherQueue dispatcher;
    private readonly Func<string, IReadOnlyList<string>, Task<string>> execute;
    private readonly CancellationTokenSource cancellation = new();
    private Task? pumpTask;

    private UiTestChannel(
        string directory,
        DispatcherQueue dispatcher,
        Func<string, IReadOnlyList<string>, Task<string>> execute)
    {
        this.directory = directory;
        this.dispatcher = dispatcher;
        this.execute = execute;
    }

    /// <summary>
    /// Start the channel if <see cref="EnableEnvironmentVariable"/> names an absolute
    /// directory. Returns null when it is unset — the normal case.
    /// </summary>
    internal static UiTestChannel? TryStart(
        DispatcherQueue dispatcher,
        Func<string, IReadOnlyList<string>, Task<string>> execute)
    {
        string? configured = Environment.GetEnvironmentVariable(EnableEnvironmentVariable);
        if (string.IsNullOrWhiteSpace(configured))
        {
            return null;
        }

        try
        {
            // Absolute only: a relative path would resolve against whatever the working
            // directory happens to be, which for a shell-launched app is not somewhere
            // anyone meant to point at.
            if (!Path.IsPathFullyQualified(configured))
            {
                AppTrace.Write("uitest/channel", $"ignored: {EnableEnvironmentVariable} must be an absolute path, got '{configured}'");
                return null;
            }

            Directory.CreateDirectory(configured);
            UiTestChannel channel = new(configured, dispatcher, execute);
            channel.pumpTask = Task.Run(() => channel.PumpAsync(channel.cancellation.Token));
            AppTrace.Write("uitest/channel", $"listening dir={configured}");
            return channel;
        }
        catch (Exception ex)
        {
            AppTrace.WriteFailure("uitest/channel", $"start dir={configured}", ex);
            return null;
        }
    }

    private async Task PumpAsync(CancellationToken token)
    {
        while (!token.IsCancellationRequested)
        {
            try
            {
                foreach (string request in Directory
                             .EnumerateFiles(directory, "*" + RequestExtension)
                             .OrderBy(path => path, StringComparer.OrdinalIgnoreCase))
                {
                    await HandleRequestAsync(request);
                }
            }
            catch (Exception ex)
            {
                AppTrace.WriteFailure("uitest/channel", "pump", ex);
            }

            try
            {
                await Task.Delay(PollInterval, token);
            }
            catch (OperationCanceledException)
            {
                return;
            }
        }
    }

    private async Task HandleRequestAsync(string requestPath)
    {
        string line;
        try
        {
            line = (await File.ReadAllTextAsync(requestPath)).Trim();
            // Delete BEFORE running: a command that hangs or crashes the pump must not be
            // replayed on every poll for the rest of the session.
            File.Delete(requestPath);
        }
        catch (IOException)
        {
            // Still being written by the other side — pick it up next pass.
            return;
        }

        string responsePath = Path.ChangeExtension(requestPath, ResponseExtension);
        string response;
        try
        {
            string[] parts = line.Split(' ', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries);
            if (parts.Length == 0)
            {
                response = "error: empty command";
            }
            else
            {
                AppTrace.Write("uitest/cmd", line);
                string result = await RunOnUiThreadAsync(parts[0], parts.Skip(1).ToArray());
                response = result;
            }
        }
        catch (Exception ex)
        {
            response = $"error: {ex.GetType().Name}: {ex.Message}";
        }

        try
        {
            await File.WriteAllTextAsync(responsePath, response + Environment.NewLine);
            AppTrace.Write("uitest/cmd-done", $"{line} -> {response.Split('\n')[0]}");
        }
        catch (Exception ex)
        {
            AppTrace.WriteFailure("uitest/channel", $"write {responsePath}", ex);
        }
    }

    private Task<string> RunOnUiThreadAsync(string command, IReadOnlyList<string> args)
    {
        TaskCompletionSource<string> completion = new(TaskCreationOptions.RunContinuationsAsynchronously);
        bool queued = dispatcher.TryEnqueue(async () =>
        {
            try
            {
                completion.TrySetResult(await execute(command, args));
            }
            catch (Exception ex)
            {
                completion.TrySetResult($"error: {ex.GetType().Name}: {ex.Message}");
            }
        });

        if (!queued)
        {
            completion.TrySetResult("error: UI thread is not accepting work");
        }

        return completion.Task;
    }

    public async ValueTask DisposeAsync()
    {
        await cancellation.CancelAsync();
        if (pumpTask is not null)
        {
            try
            {
                await pumpTask;
            }
            catch (OperationCanceledException)
            {
            }
        }

        cancellation.Dispose();
    }
}
