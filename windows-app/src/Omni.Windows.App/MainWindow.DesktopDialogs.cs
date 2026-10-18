using System.Runtime.InteropServices;
using System.Text;

namespace Omni.Windows.App;

public sealed partial class MainWindow
{
    private const int OfnOverwritePrompt = 0x00000002;
    private const int OfnHideReadOnly = 0x00000004;
    private const int OfnNoChangeDir = 0x00000008;
    private const int OfnPathMustExist = 0x00000800;
    private const int OfnFileMustExist = 0x00001000;
    private const int OfnExplorer = 0x00080000;

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private sealed class OpenFileName
    {
        public int lStructSize;
        public nint hwndOwner;
        public nint hInstance;
        [MarshalAs(UnmanagedType.LPWStr)]
        public string lpstrFilter = string.Empty;
        [MarshalAs(UnmanagedType.LPWStr)]
        public string? lpstrCustomFilter;
        public int nMaxCustFilter;
        public int nFilterIndex;
        [MarshalAs(UnmanagedType.LPWStr)]
        public string lpstrFile = string.Empty;
        public int nMaxFile;
        [MarshalAs(UnmanagedType.LPWStr)]
        public string? lpstrFileTitle;
        public int nMaxFileTitle;
        [MarshalAs(UnmanagedType.LPWStr)]
        public string? lpstrInitialDir;
        [MarshalAs(UnmanagedType.LPWStr)]
        public string? lpstrTitle;
        public int Flags;
        public short nFileOffset;
        public short nFileExtension;
        [MarshalAs(UnmanagedType.LPWStr)]
        public string? lpstrDefExt;
        public nint lCustData;
        public nint lpfnHook;
        [MarshalAs(UnmanagedType.LPWStr)]
        public string? lpTemplateName;
        public nint pvReserved;
        public int dwReserved;
        public int FlagsEx;
    }

    [DllImport("comdlg32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool GetOpenFileName([In, Out] OpenFileName openFileName);

    [DllImport("comdlg32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool GetSaveFileName([In, Out] OpenFileName openFileName);

    [DllImport("comdlg32.dll", CharSet = CharSet.Unicode)]
    private static extern int CommDlgExtendedError();

    private sealed record DesktopFileDialogFilter(string Label, string Pattern);

    private nint GetRequiredWindowHandle()
    {
        nint handle = WindowNative.GetWindowHandle(this);
        if (handle == IntPtr.Zero)
        {
            throw new InvalidOperationException("Не удалось определить активное окно приложения.");
        }

        return handle;
    }

    private static string BuildDialogFilter(IReadOnlyList<DesktopFileDialogFilter> filters)
    {
        if (filters.Count == 0)
        {
            return "Все файлы (*.*)\0*.*\0\0";
        }

        StringBuilder builder = new();
        foreach (DesktopFileDialogFilter filter in filters)
        {
            builder.Append(filter.Label);
            builder.Append('\0');
            builder.Append(filter.Pattern);
            builder.Append('\0');
        }

        builder.Append('\0');
        return builder.ToString();
    }

    private static string NormalizeInitialDirectory(string? initialDirectory)
        => !string.IsNullOrWhiteSpace(initialDirectory) && Directory.Exists(initialDirectory)
            ? initialDirectory
            : Environment.GetFolderPath(Environment.SpecialFolder.MyDocuments);

    private static string FinalizeDialogResult(string fileBuffer)
    {
        string result = fileBuffer.TrimEnd('\0');
        if (string.IsNullOrWhiteSpace(result))
        {
            throw new InvalidOperationException("Диалог выбора файла не вернул путь.");
        }

        return result;
    }

    private string? ShowDesktopOpenFileDialog(
        string title,
        IReadOnlyList<DesktopFileDialogFilter> filters,
        string? initialDirectory = null)
    {
        OpenFileName dialog = new()
        {
            lStructSize = Marshal.SizeOf<OpenFileName>(),
            hwndOwner = GetRequiredWindowHandle(),
            lpstrFilter = BuildDialogFilter(filters),
            lpstrFile = new string('\0', 1024),
            nMaxFile = 1024,
            lpstrInitialDir = NormalizeInitialDirectory(initialDirectory),
            lpstrTitle = title,
            Flags = OfnExplorer | OfnHideReadOnly | OfnNoChangeDir | OfnPathMustExist | OfnFileMustExist,
        };

        return GetOpenFileName(dialog)
            ? FinalizeDialogResult(dialog.lpstrFile)
            : CommDlgExtendedError() == 0
                ? null
                : throw new InvalidOperationException(
                    $"Диалог выбора файла завершился с кодом ошибки 0x{CommDlgExtendedError():X4}.");
    }

    private string? ShowDesktopSaveFileDialog(
        string title,
        string suggestedFileName,
        IReadOnlyList<DesktopFileDialogFilter> filters,
        string? defaultExtension = null,
        string? initialDirectory = null)
    {
        string initialDir = NormalizeInitialDirectory(initialDirectory);
        OpenFileName dialog = new()
        {
            lStructSize = Marshal.SizeOf<OpenFileName>(),
            hwndOwner = GetRequiredWindowHandle(),
            lpstrFilter = BuildDialogFilter(filters),
            lpstrFile = Path.Combine(initialDir, suggestedFileName).PadRight(1024, '\0'),
            nMaxFile = 1024,
            lpstrInitialDir = initialDir,
            lpstrTitle = title,
            lpstrDefExt = string.IsNullOrWhiteSpace(defaultExtension)
                ? null
                : defaultExtension.TrimStart('.'),
            Flags = OfnExplorer | OfnHideReadOnly | OfnNoChangeDir | OfnPathMustExist | OfnOverwritePrompt,
        };

        return GetSaveFileName(dialog)
            ? FinalizeDialogResult(dialog.lpstrFile)
            : CommDlgExtendedError() == 0
                ? null
                : throw new InvalidOperationException(
                    $"Диалог сохранения файла завершился с кодом ошибки 0x{CommDlgExtendedError():X4}.");
    }
}
