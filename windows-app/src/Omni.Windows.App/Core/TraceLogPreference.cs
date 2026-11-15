using System.Text.Json.Nodes;

namespace Omni.Windows.App.Core;

/// <summary>
/// «Журнал для разработчика» (Settings → «Для опытных»): whether the diagnostic trace is written, per
/// device in device-preferences.json (key <see cref="Key"/>). Off by default in a product build — the
/// trace names peers, addresses and timings — and on in a dev build, which always wrote it. Read by
/// the app before the window exists, so it parses the file itself instead of going through the
/// window's preference helpers.
/// </summary>
internal static class TraceLogPreference
{
    internal const string Key = "traceLog";

#if OMNI_DEV_TOOLS
    internal const bool Default = true;
#else
    internal const bool Default = false;
#endif

    internal static bool Load(string appDirectory)
    {
        try
        {
            string path = Path.Combine(appDirectory, "device-preferences.json");
            if (File.Exists(path)
                && JsonNode.Parse(File.ReadAllText(path)) is JsonObject document
                && document[Key] is JsonValue value
                && value.TryGetValue(out bool stored))
            {
                return stored;
            }
        }
        catch
        {
            // An unreadable file: the default, as for every other device preference.
        }

        return Default;
    }

    /// <summary>The line under the switch: where the file is, or what overrides the switch.</summary>
    internal static string StatusText(string appDirectory, bool on, string? environmentFile)
        => environmentFile is not null
            ? $"Включён переменной OMNI_TRACE_FILE: {environmentFile}"
            : on
                ? $"Пишется в {AppTrace.LogFilePath(appDirectory)} (до 32 МБ, затем старая часть уходит в trace.log.1)."
                : "Включите, повторите проблему и отправьте файл trace.log разработчику.";
}
