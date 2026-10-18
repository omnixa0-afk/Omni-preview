using System.Linq;
using System.Text.Json;

namespace Omni.Windows.App.Core;

internal sealed partial class RustBridge
{
    /// The portable data folder, next to the executable. Omni stays portable — copy the folder, keep
    /// your identity; several copies side by side stay isolated; nothing is written to the user
    /// profile behind the user's back — but it lands in ONE directory instead of scattering a db,
    /// its journals and a dozen settings sidecars around whatever folder the exe was dropped into.
    private const string DataDirectoryName = "OmniData";

    internal static string GetDefaultAppDirectory()
    {
        string? overrideDirectory = Environment.GetEnvironmentVariable("OMNI_APPDATA_DIR");
        if (!string.IsNullOrWhiteSpace(overrideDirectory))
        {
            return overrideDirectory;
        }

        string executableDirectory = GetExecutableDirectory();
        string dataDirectory = Path.Combine(executableDirectory, DataDirectoryName);
        MigrateLooseDataIntoDataDirectory(executableDirectory, dataDirectory);
        return dataDirectory;
    }

    private static string GetExecutableDirectory()
    {
        string? processPath = Environment.ProcessPath;
        if (!string.IsNullOrWhiteSpace(processPath))
        {
            string? directory = Path.GetDirectoryName(processPath);
            if (!string.IsNullOrWhiteSpace(directory))
            {
                return Path.GetFullPath(directory);
            }
        }

        return Path.GetFullPath(AppContext.BaseDirectory);
    }

    /// One-time move of a pre-OmniData layout (db + sidecars sitting loose next to the exe) into the
    /// data folder, so an existing install keeps its profile instead of silently booting into a blank
    /// one while its data sits right there.
    ///
    /// Only runs when there is a loose db and the data folder has none — never overwrites, and does
    /// nothing at all on a fresh install. Best-effort: a file we cannot move must not block startup;
    /// worst case the user lands on onboarding with the old files still on disk.
    private static void MigrateLooseDataIntoDataDirectory(string executableDirectory, string dataDirectory)
    {
        try
        {
            string looseDatabase = Path.Combine(executableDirectory, "omni.db");
            if (!File.Exists(looseDatabase) || File.Exists(Path.Combine(dataDirectory, "omni.db")))
            {
                return;
            }

            Directory.CreateDirectory(dataDirectory);
            // Materialize BEFORE moving: the enumerator walks the very directory we are mutating, and
            // Windows' find-next can skip entries out from under it — a skipped file is data left
            // orphaned outside the profile.
            foreach (string source in EnumerateLooseDataPaths(executableDirectory).ToList())
            {
                string destination = Path.Combine(dataDirectory, Path.GetFileName(source));
                if (Directory.Exists(source))
                {
                    if (!Directory.Exists(destination))
                    {
                        Directory.Move(source, destination);
                    }
                }
                else if (!File.Exists(destination))
                {
                    File.Move(source, destination);
                }
            }
        }
        catch (Exception)
        {
            // Best-effort by design (see the summary) — never block startup on the move.
        }
    }

    /// The pre-OmniData footprint: everything keyed by the db file name (`omni.db`, its journals and
    /// every `omni.db.*` sidecar — same prefix rule the core's wipe uses), the fixed-name sidecars the
    /// shell writes, and the data folders. Deliberately an allowlist: the exe's own directory holds
    /// the app's DLLs and *.json runtime configs, which must not be swept up.
    private static IEnumerable<string> EnumerateLooseDataPaths(string executableDirectory)
    {
        foreach (string path in Directory.EnumerateFiles(executableDirectory, "omni.db*"))
        {
            yield return path;
        }

        foreach (string fileName in new[]
                 {
                     "transport-mode.json",
                     "node-mode.json",
                     "discovery-publish-mode.json",
                     "discovery-probe-mode.json",
                     "discovery-policy.json",
                     "discovery-network-config.json",
                     "internet-routing.json",
                     "stun-servers.json",
                     "connection-privacy-mode.json",
                     "carrier-policy.json",
                     "transport-identity.key",
                 })
        {
            string path = Path.Combine(executableDirectory, fileName);
            if (File.Exists(path))
            {
                yield return path;
            }
        }

        foreach (string directoryName in new[] { "attachments", "profile-media", "workspaces" })
        {
            string path = Path.Combine(executableDirectory, directoryName);
            if (Directory.Exists(path))
            {
                yield return path;
            }
        }
    }

    internal static string GetWorkspaceAppDirectory(string workspaceId)
    {
        string cleanedWorkspaceId = SanitizeWorkspaceId(workspaceId);
        return Path.Combine(GetDefaultAppDirectory(), "workspaces", cleanedWorkspaceId);
    }

    internal static string PrepareWorkspaceAppDirectory(string workspaceId, TransportMode transportMode)
    {
        string workspaceDirectory = GetWorkspaceAppDirectory(workspaceId);
        Directory.CreateDirectory(workspaceDirectory);
        SeedWorkspaceDefaultSidecars(workspaceDirectory, transportMode);
        return workspaceDirectory;
    }

    internal static TransportMode ReadTransportModeFromSidecar(string appDirectory)
        => ReadEnumSidecar(
            appDirectory,
            GetTransportModeSidecarPath,
            TransportMode.Quic,
            "transport_mode",
            TransportModeExtensions.FromDbValue);

    internal static void WriteTransportModeSidecar(string appDirectory, TransportMode transportMode)
        => WriteSidecarValue(appDirectory, GetTransportModeSidecarPath, transportMode.ToDbValue());

    // A copied/isolated workspace inherits the transport mode — the one sidecar the core still reads.
    // The pre-mesh discovery and STUN sidecars went with that discovery (no-friend-tier.md, part 2).
    private void ApplyConfiguredSidecarsToDirectory(string appDirectory)
        => WriteTransportModeSidecar(appDirectory, GetConfiguredTransportMode());

    private static void SeedWorkspaceDefaultSidecars(string appDirectory, TransportMode transportMode)
        => WriteTransportModeSidecar(appDirectory, transportMode);

    private static TEnum ReadEnumSidecar<TEnum>(
        string appDirectory,
        Func<string, string> sidecarPathFactory,
        TEnum defaultValue,
        string jsonPropertyName,
        Func<string, TEnum> parser)
    {
        string sidecarPath = sidecarPathFactory(appDirectory);
        if (!File.Exists(sidecarPath))
        {
            return defaultValue;
        }

        string raw = File.ReadAllText(sidecarPath).Trim();
        if (string.IsNullOrWhiteSpace(raw))
        {
            return defaultValue;
        }

        return parser(TryReadSidecarJsonStringProperty(raw, jsonPropertyName) ?? raw);
    }

    private static void WriteSidecarValue(
        string appDirectory,
        Func<string, string> sidecarPathFactory,
        string value)
    {
        Directory.CreateDirectory(appDirectory);
        File.WriteAllText(sidecarPathFactory(appDirectory), value);
    }

    private static string? TryReadSidecarJsonStringProperty(string raw, string propertyName)
    {
        if (!raw.StartsWith('{'))
        {
            return null;
        }

        try
        {
            using JsonDocument document = JsonDocument.Parse(raw);
            if (document.RootElement.ValueKind != JsonValueKind.Object)
            {
                return null;
            }

            if (document.RootElement.TryGetProperty(propertyName, out JsonElement propertyValue) &&
                propertyValue.ValueKind == JsonValueKind.String)
            {
                return propertyValue.GetString();
            }
        }
        catch (JsonException)
        {
            return null;
        }

        return null;
    }

    private static string GetTransportModeSidecarPath(string appDirectory)
        => Path.Combine(appDirectory, "transport-mode.json");

    private static string GetWorkspaceRootDirectory(string appDirectory)
    {
        string normalizedDirectory = Path.GetFullPath(appDirectory);
        string? parentDirectory = Path.GetDirectoryName(normalizedDirectory);
        if (string.Equals(Path.GetFileName(parentDirectory), "workspaces", StringComparison.OrdinalIgnoreCase))
        {
            return Path.GetDirectoryName(parentDirectory) ?? normalizedDirectory;
        }

        return normalizedDirectory;
    }

    private static string GetCloneRootDirectory(string appDirectory)
    {
        string normalizedDirectory = Path.GetFullPath(appDirectory);
        string? parentDirectory = Path.GetDirectoryName(normalizedDirectory);
        if (string.IsNullOrWhiteSpace(parentDirectory))
        {
            parentDirectory = normalizedDirectory;
        }

        string directoryName = Path.GetFileName(normalizedDirectory.TrimEnd(
            Path.DirectorySeparatorChar,
            Path.AltDirectorySeparatorChar));
        if (string.IsNullOrWhiteSpace(directoryName))
        {
            directoryName = "Omni";
        }

        return Path.Combine(parentDirectory, $"{directoryName}.clones");
    }

    private void CloneAppDirectorySnapshot(string sourceDirectory, string destinationDirectory)
    {
        string sourceFullPath = Path.GetFullPath(sourceDirectory);
        string destinationFullPath = Path.GetFullPath(destinationDirectory);
        if (string.Equals(sourceFullPath, destinationFullPath, StringComparison.OrdinalIgnoreCase))
        {
            throw new InvalidOperationException("Каталог копии совпадает с исходным каталогом приложения.");
        }

        if (destinationFullPath.StartsWith(
                sourceFullPath.TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar) + Path.DirectorySeparatorChar,
                StringComparison.OrdinalIgnoreCase) ||
            destinationFullPath.StartsWith(
                sourceFullPath.TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar) + Path.AltDirectorySeparatorChar,
                StringComparison.OrdinalIgnoreCase))
        {
            throw new InvalidOperationException("Каталог копии не должен лежать внутри исходного каталога приложения.");
        }

        PrepareEmptyDirectory(destinationFullPath);
        ExportDatabaseBackupBundleToPath(
            Path.Combine(destinationFullPath, "omni.db"),
            preferLiveSnapshot: IsInitialized);
    }

    private void ExportDatabaseBackupBundleToPath(string destinationDatabasePath, bool preferLiveSnapshot)
    {
        string requestJson = JsonSerializer.Serialize(
            new ExportCurrentDatabaseBackupRequest(destinationDatabasePath),
            OmniJsonSerializerContext.Default.ExportCurrentDatabaseBackupRequest);
        string configJson = JsonSerializer.Serialize(
            new InitConfigRequest(GetDatabasePath()),
            OmniJsonSerializerContext.Default.InitConfigRequest);

        if (preferLiveSnapshot && IsInitialized)
        {
            string payload = CallRaw(
                () => NativeMethods.omni_identity_export_current_database_backup_bundle(handle, requestJson),
                "identity_export_current_database_backup_bundle");
            ThrowIfRustError(payload, "identity_export_current_database_backup_bundle");
            return;
        }

        CallNoHandle(
            () => NativeMethods.omni_app_export_database_backup_bundle(configJson, requestJson),
            "export_database_backup_bundle");
    }

    private static void PrepareEmptyDirectory(string path)
    {
        if (Directory.Exists(path))
        {
            Directory.Delete(path, recursive: true);
        }

        Directory.CreateDirectory(path);
    }

    private static string NormalizeAppDirectory(string? appDirectory)
    {
        string resolved = string.IsNullOrWhiteSpace(appDirectory)
            ? GetDefaultAppDirectory()
            : appDirectory.Trim();
        return Path.GetFullPath(resolved);
    }

    private static string SanitizeWorkspaceId(string workspaceId)
    {
        string trimmed = workspaceId.Trim();
        if (trimmed.Length == 0)
        {
            throw new InvalidOperationException("workspace id must not be empty");
        }

        char[] invalidChars = Path.GetInvalidFileNameChars();
        return new string(trimmed.Select(ch => invalidChars.Contains(ch) ? '_' : ch).ToArray());
    }
}
