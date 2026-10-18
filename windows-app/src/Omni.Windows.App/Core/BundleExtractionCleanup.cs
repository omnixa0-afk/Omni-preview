namespace Omni.Windows.App.Core;

/// <summary>
/// Removes what earlier versions of the one-file build left in the .NET extraction folder.
///
/// The standalone build unpacks its native libraries to <c>%TEMP%\.net\Omni.Windows.App\&lt;hash&gt;\</c>
/// on first launch, one folder per build, and .NET never removes the folders of older builds: every
/// update left ~68 MB on the system drive. Found 2026-10-10 with 163 such folders, 11 GB, on a disk that
/// had run out of space.
///
/// A folder still in use (another instance, maybe an older version, or its courier child processes)
/// has open files, and Windows refuses to rename a folder with an open file in it. So each folder is
/// first renamed, then deleted: one that cannot be renamed is in use and is left as it is, and a
/// folder half-deleted under its name is never left for a later launch of that version to trust.
/// </summary>
internal static class BundleExtractionCleanup
{
    private const string RemovingSuffix = ".removing";

    internal static void RemoveStaleCopiesInBackground()
    {
        string? current = CurrentExtractionDirectory();
        if (current is null)
        {
            return; // not a one-file build: the libraries sit next to the exe
        }

        _ = Task.Run(() => RemoveStaleCopies(current));
    }

    /// <summary>The extraction folder this process runs from, or null when it does not run from one.</summary>
    private static string? CurrentExtractionDirectory()
    {
        // The loaded core names it exactly; before it loads, the base directory is that same folder.
        string? library = NativeMethods.LoadedLibraryPath;
        string? folder = library is null
            ? Path.TrimEndingDirectorySeparator(Path.GetFullPath(AppContext.BaseDirectory))
            : Path.GetDirectoryName(Path.GetFullPath(library));
        string? appFolder = folder is null ? null : Path.GetDirectoryName(folder);
        string? dotnetFolder = appFolder is null ? null : Path.GetDirectoryName(appFolder);
        return dotnetFolder is not null
            && string.Equals(Path.GetFileName(dotnetFolder), ".net", StringComparison.OrdinalIgnoreCase)
            ? folder
            : null;
    }

    internal static int RemoveStaleCopies(string current)
    {
        string? parent = Path.GetDirectoryName(current);
        if (parent is null)
        {
            return 0;
        }

        int removed = 0;
        foreach (string folder in Directory.EnumerateDirectories(parent))
        {
            if (string.Equals(folder, current, StringComparison.OrdinalIgnoreCase))
            {
                continue;
            }

            string doomed = folder;
            if (!folder.EndsWith(RemovingSuffix, StringComparison.OrdinalIgnoreCase))
            {
                doomed = folder + "." + Guid.NewGuid().ToString("N")[..8] + RemovingSuffix;
                try
                {
                    Directory.Move(folder, doomed);
                }
                catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
                {
                    continue; // in use by a running instance
                }
            }

            try
            {
                Directory.Delete(doomed, recursive: true);
                removed++;
            }
            catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
            {
                // Renamed, so no launch will ever reuse it; the next start finishes the job.
                AppTrace.Write("app/extract-cleanup", $"could not remove {doomed}: {ex.Message}");
            }
        }

        if (removed > 0)
        {
            AppTrace.Write("app/extract-cleanup", $"removed {removed} copy(ies) of older builds from {parent}");
        }

        return removed;
    }
}
