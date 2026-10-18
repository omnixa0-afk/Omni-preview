using Omni.Windows.App.Core;

namespace Omni.Windows.App;

internal sealed class BackupImportSession
{
    private string? selectedBackupPath;

    public AccountExportManifestV1? Manifest { get; private set; }

    public string? InspectionToken { get; private set; }

    public void SelectFile(string fullPath)
    {
        string normalizedPath = NormalizePath(fullPath);
        if (string.Equals(selectedBackupPath, normalizedPath, StringComparison.OrdinalIgnoreCase))
        {
            return;
        }

        selectedBackupPath = normalizedPath;
        Manifest = null;
        InspectionToken = null;
    }

    public void RecordInspection(string fullPath, AccountBackupInspection inspection)
    {
        selectedBackupPath = NormalizePath(fullPath);
        Manifest = inspection.Manifest;
        InspectionToken = inspection.InspectionToken;
    }

    public string GetRequiredInspectionToken(string fullPath)
    {
        string normalizedPath = NormalizePath(fullPath);
        if (!string.Equals(selectedBackupPath, normalizedPath, StringComparison.OrdinalIgnoreCase) ||
            string.IsNullOrWhiteSpace(InspectionToken))
        {
            throw new InvalidOperationException("Проверка резервной копии устарела. Проверьте файл повторно перед импортом.");
        }

        return InspectionToken;
    }

    public void Clear()
    {
        selectedBackupPath = null;
        Manifest = null;
        InspectionToken = null;
    }

    private static string NormalizePath(string fullPath)
    {
        if (string.IsNullOrWhiteSpace(fullPath))
        {
            throw new InvalidOperationException("Путь к резервной копии не задан.");
        }

        return Path.GetFullPath(fullPath);
    }
}
