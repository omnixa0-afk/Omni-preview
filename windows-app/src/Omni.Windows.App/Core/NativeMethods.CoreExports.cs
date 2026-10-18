using System.Runtime.InteropServices;

namespace Omni.Windows.App.Core;

internal static partial class NativeMethods
{
    private static OmniAppInitDelegate? omniAppInit;
    private static OmniAppLastInitErrorDelegate? omniAppLastInitError;
    private static OmniAppFreeDelegate? omniAppFree;
    private static OmniAppPingDelegate? omniAppPing;
    private static OmniDaemonStartDelegate? omniDaemonStart;
    private static OmniInspectDatabaseBootstrapDelegate? omniInspectDatabaseBootstrap;
    private static OmniResetLocalDataDelegate? omniResetLocalData;
    private static OmniDatabaseLockStateDelegate? omniDatabaseLockState;
    private static OmniUnlockDatabaseWithMnemonicDelegate? omniUnlockDatabaseWithMnemonic;
    private static OmniExportDatabaseBackupBundleDelegate? omniAppExportDatabaseBackupBundle;
    private static OmniImportDatabaseBackupBundleDelegate? omniAppImportDatabaseBackupBundle;
    private static OmniRunHelperDelegate? omniRunHelper;
    private static OmniTraceWriteDelegate? omniTraceWrite;

    private static void LoadCoreAppExports()
    {
        LoadExport(ref omniAppInit, nameof(omni_app_init));
        LoadExport(ref omniAppLastInitError, nameof(omni_app_last_init_error));
        LoadExport(ref omniAppFree, nameof(omni_app_free));
        LoadExport(ref omniAppPing, nameof(omni_app_ping));
        LoadExport(ref omniDaemonStart, nameof(omni_daemon_start));
        LoadExport(ref omniInspectDatabaseBootstrap, nameof(omni_app_inspect_database_bootstrap));
        LoadExport(ref omniResetLocalData, nameof(omni_app_reset_local_data));
        LoadExport(ref omniDatabaseLockState, nameof(omni_app_database_lock_state));
        LoadExport(ref omniUnlockDatabaseWithMnemonic, nameof(omni_app_unlock_database_with_mnemonic));
        LoadExport(ref omniAppExportDatabaseBackupBundle, nameof(omni_app_export_database_backup_bundle));
        LoadExport(ref omniAppImportDatabaseBackupBundle, nameof(omni_app_import_database_backup_bundle));
        LoadExport(ref omniRunHelper, nameof(omni_run_helper));
        LoadExport(ref omniTraceWrite, nameof(omni_trace_write));
    }

    internal static IntPtr omni_app_init(string configJson)
    {
        EnsureLoaded();
        return omniAppInit!(configJson);
    }

    /// <summary>
    /// Почему последняя инициализация вернула пустой дескриптор. Пустая строка — причины нет.
    /// </summary>
    internal static string omni_app_last_init_error()
    {
        EnsureLoaded();
        IntPtr ptr = omniAppLastInitError!();
        if (ptr == IntPtr.Zero)
        {
            return string.Empty;
        }

        try
        {
            return Marshal.PtrToStringUTF8(ptr) ?? string.Empty;
        }
        finally
        {
            omni_string_free(ptr);
        }
    }

    internal static void omni_app_free(IntPtr handle)
    {
        EnsureLoaded();
        omniAppFree!(handle);
    }

    internal static IntPtr omni_app_ping(IntPtr handle)
    {
        EnsureLoaded();
        return omniAppPing!(handle);
    }

    internal static IntPtr omni_daemon_start(IntPtr handle, string endpoint)
    {
        EnsureLoaded();
        return omniDaemonStart!(handle, endpoint);
    }

    internal static IntPtr omni_app_inspect_database_bootstrap(string configJson)
    {
        EnsureLoaded();
        return omniInspectDatabaseBootstrap!(configJson);
    }

    internal static IntPtr omni_app_reset_local_data(string configJson)
    {
        EnsureLoaded();
        return omniResetLocalData!(configJson);
    }

    internal static IntPtr omni_app_database_lock_state(string configJson)
    {
        EnsureLoaded();
        return omniDatabaseLockState!(configJson);
    }

    internal static IntPtr omni_app_unlock_database_with_mnemonic(string configJson, string requestJson)
    {
        EnsureLoaded();
        return omniUnlockDatabaseWithMnemonic!(configJson, requestJson);
    }

    internal static IntPtr omni_app_export_database_backup_bundle(string configJson, string requestJson)
    {
        EnsureLoaded();
        return omniAppExportDatabaseBackupBundle!(configJson, requestJson);
    }

    internal static IntPtr omni_app_import_database_backup_bundle(string configJson, string requestJson)
    {
        EnsureLoaded();
        return omniAppImportDatabaseBackupBundle!(configJson, requestJson);
    }

    /// Run a courier helper IN THIS PROCESS and return its exit code. Blocks for the helper's whole
    /// lifetime — the caller is a `--run-helper` re-exec of the app that exists only to be that
    /// helper (see Core/HelperMode.cs), never the UI process.
    internal static int omni_run_helper(string kind, string argsJson)
    {
        EnsureLoaded();
        return omniRunHelper!(kind, argsJson);
    }

    /// Append a line to the `OMNI_TRACE_FILE` sink through the core — the file's only writer (see
    /// Core/AppTrace.cs). A no-op in the core when the sink is off.
    internal static void omni_trace_write(string category, string message)
    {
        EnsureLoaded();
        omniTraceWrite!(category, message);
    }

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate void OmniTraceWriteDelegate(
        [MarshalAs(UnmanagedType.LPUTF8Str)] string category,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string message);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniAppInitDelegate([MarshalAs(UnmanagedType.LPUTF8Str)] string configJson);

    private delegate IntPtr OmniAppLastInitErrorDelegate();

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate void OmniAppFreeDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniAppPingDelegate(IntPtr handle);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniDaemonStartDelegate(
        IntPtr handle,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string endpoint);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniInspectDatabaseBootstrapDelegate([MarshalAs(UnmanagedType.LPUTF8Str)] string configJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniResetLocalDataDelegate([MarshalAs(UnmanagedType.LPUTF8Str)] string configJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniDatabaseLockStateDelegate([MarshalAs(UnmanagedType.LPUTF8Str)] string configJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniUnlockDatabaseWithMnemonicDelegate(
        [MarshalAs(UnmanagedType.LPUTF8Str)] string configJson,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate int OmniRunHelperDelegate(
        [MarshalAs(UnmanagedType.LPUTF8Str)] string kind,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string argsJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniExportDatabaseBackupBundleDelegate(
        [MarshalAs(UnmanagedType.LPUTF8Str)] string configJson,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate IntPtr OmniImportDatabaseBackupBundleDelegate(
        [MarshalAs(UnmanagedType.LPUTF8Str)] string configJson,
        [MarshalAs(UnmanagedType.LPUTF8Str)] string requestJson);
}
