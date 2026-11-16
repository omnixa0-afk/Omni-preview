using System.Collections.Generic;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text.Json;
using System.Text.Json.Serialization;
using System.Text.Json.Serialization.Metadata;
using System.Threading;

namespace Omni.Windows.App.Core;

internal sealed partial class RustBridge : IDisposable
{
    private readonly object syncRoot = new();
    /// <summary>
    /// The core's lifetime against <see cref="CallRawLong"/>: long workspace calls (git over a folder of
    /// models) hold it shared and run outside <see cref="syncRoot"/>, so the rest of the shell does not
    /// wait for them; freeing the core takes it exclusively. The core serializes its own state.
    /// </summary>
    private readonly ReaderWriterLockSlim handleLifetime = new(LockRecursionPolicy.SupportsRecursion);
    private readonly string appDirectory;
    private readonly SidecarSettingsCache sidecarSettingsCache = new();
    private IntPtr handle = IntPtr.Zero;
    private FileStream? appDirectoryLock;

    public RustBridge(string? appDirectory = null)
    {
        this.appDirectory = NormalizeAppDirectory(appDirectory);
        SubscribeNetworkInventoryEvents();
    }

    public bool IsInitialized
        => Interlocked.CompareExchange(ref handle, IntPtr.Zero, IntPtr.Zero) != IntPtr.Zero;

    public string AppDirectory => appDirectory;

    public string DatabasePath => GetDatabasePath();

    /// <summary>
    /// A native call that takes the core's handle and runs outside <see cref="syncRoot"/> (mesh, media):
    /// it holds <see cref="handleLifetime"/> shared until the call returns, and reads the handle inside
    /// it — so freeing the core waits for the call instead of freeing it under it (F01, audit
    /// 2026-11-09: a handle read before, then freed by closing or a backup import, went to the core as
    /// freed memory). <paramref name="call"/> must not call back into the bridge's
    /// <see cref="syncRoot"/> calls: freeing takes <see cref="syncRoot"/> first, then this lock.
    /// </summary>
    internal T WithCoreHandle<T>(Func<IntPtr, T> call)
    {
        T result = default!;
        if (!TryWithCoreHandle(current => result = call(current)))
        {
            throw new InvalidOperationException("Omni ещё запускается — повторите через несколько секунд.");
        }

        return result;
    }

    /// <summary><see cref="WithCoreHandle{T}"/> for a call that has nothing to do once the core is gone: false then.</summary>
    internal bool TryWithCoreHandle(Action<IntPtr> call)
    {
        handleLifetime.EnterReadLock();
        try
        {
            IntPtr current = Volatile.Read(ref handle);
            if (current == IntPtr.Zero)
            {
                return false;
            }

            call(current);
            return true;
        }
        finally
        {
            handleLifetime.ExitReadLock();
        }
    }

    public void Init()
    {
        lock (syncRoot)
        {
            InitCore();
        }
    }

    public DatabaseBootstrapSummary InspectDatabaseBootstrap()
    {
        string configJson = JsonSerializer.Serialize(
            new InitConfigRequest(GetDatabasePath()),
            OmniJsonSerializerContext.Default.InitConfigRequest);

        return CallJsonNoHandle(
            () => NativeMethods.omni_app_inspect_database_bootstrap(configJson),
            OmniJsonSerializerContext.Default.DatabaseBootstrapSummary,
            "inspect_database_bootstrap");
    }

    /// <summary>
    /// База открывается на этой машине как есть, или нужна фраза восстановления: папка данных
    /// перенесена с другого компьютера (ключ базы запечатан DPAPI той машины).
    /// </summary>
    public bool DatabaseNeedsRecoveryPhrase()
    {
        string configJson = JsonSerializer.Serialize(
            new InitConfigRequest(GetDatabasePath()),
            OmniJsonSerializerContext.Default.InitConfigRequest);

        DatabaseLockStateResponse response = CallJsonNoHandle(
            () => NativeMethods.omni_app_database_lock_state(configJson),
            OmniJsonSerializerContext.Default.DatabaseLockStateResponse,
            "database_lock_state");
        return response.State == "needs_recovery_phrase";
    }

    public void UnlockDatabaseWithMnemonic(string mnemonic)
    {
        string configJson = JsonSerializer.Serialize(
            new InitConfigRequest(GetDatabasePath()),
            OmniJsonSerializerContext.Default.InitConfigRequest);
        string requestJson = JsonSerializer.Serialize(
            new UnlockDatabaseWithMnemonicRequest(mnemonic),
            OmniJsonSerializerContext.Default.UnlockDatabaseWithMnemonicRequest);

        CallNoHandle(
            () => NativeMethods.omni_app_unlock_database_with_mnemonic(configJson, requestJson),
            "unlock_database_with_mnemonic");
    }

    public void ResetLocalData()
    {
        string configJson = JsonSerializer.Serialize(
            new InitConfigRequest(GetDatabasePath()),
            OmniJsonSerializerContext.Default.InitConfigRequest);

        CallNoHandle(
            () => NativeMethods.omni_app_reset_local_data(configJson),
            "reset_local_data");
    }

    public void ImportDatabaseBackup(string backupDatabasePath, string inspectionToken)
    {
        string destinationPath = GetDatabasePath();
        if (string.Equals(
                Path.GetFullPath(backupDatabasePath),
                Path.GetFullPath(destinationPath),
                StringComparison.OrdinalIgnoreCase))
        {
            throw new InvalidOperationException("Файл резервной копии не должен совпадать с текущей базой данных.");
        }

        lock (syncRoot)
        {
            FreeHandle();
        }

        string configJson = JsonSerializer.Serialize(
            new InitConfigRequest(destinationPath),
            OmniJsonSerializerContext.Default.InitConfigRequest);
        string requestJson = JsonSerializer.Serialize(
            new ImportDatabaseBackupRequest(backupDatabasePath, inspectionToken),
            OmniJsonSerializerContext.Default.ImportDatabaseBackupRequest);

        CallNoHandle(
            () => NativeMethods.omni_app_import_database_backup_bundle(configJson, requestJson),
            "import_database_backup_bundle");
    }

    public void ExportCurrentDatabaseBackupBundle(string destinationPath)
    {
        string requestJson = JsonSerializer.Serialize(
            new ExportCurrentDatabaseBackupRequest(destinationPath),
            OmniJsonSerializerContext.Default.ExportCurrentDatabaseBackupRequest);

        if (IsInitialized)
        {
            string payload = CallRaw(
                () => NativeMethods.omni_identity_export_current_database_backup_bundle(handle, requestJson),
                "identity_export_current_database_backup_bundle");
            ThrowIfRustError(payload, "identity_export_current_database_backup_bundle");
            return;
        }

        string configJson = JsonSerializer.Serialize(
            new InitConfigRequest(GetDatabasePath()),
            OmniJsonSerializerContext.Default.InitConfigRequest);
        CallNoHandle(
            () => NativeMethods.omni_app_export_database_backup_bundle(configJson, requestJson),
            "export_database_backup_bundle");
    }

    public string Ping()
        => CallRaw(() => NativeMethods.omni_app_ping(handle), "ping");

    public void Dispose()
    {
        UnsubscribeNetworkInventoryEvents();
        lock (syncRoot)
        {
            DisposeCore();
        }
    }

    private T CallJson<T>(Func<IntPtr> nativeCall, JsonTypeInfo<T> typeInfo, string operation)
    {
        string payload = CallRaw(nativeCall, operation);
        ThrowIfRustError(payload, operation);

        long started = BridgeCost.Start();
        T? response = JsonSerializer.Deserialize(payload, typeInfo);
        BridgeCost.Record("json", started);
        if (response is null)
        {
            throw new InvalidOperationException("Rust core returned an empty JSON payload.");
        }

        return response;
    }

    private T? CallOptionalJson<T>(Func<IntPtr> nativeCall, JsonTypeInfo<T> typeInfo, string operation)
        where T : class
    {
        string payload = CallRaw(nativeCall, operation);
        ThrowIfRustError(payload, operation);

        if (string.Equals(payload.Trim(), "null", StringComparison.Ordinal))
        {
            return null;
        }

        return JsonSerializer.Deserialize(payload, typeInfo);
    }

    private T CallJsonNoHandle<T>(Func<IntPtr> nativeCall, JsonTypeInfo<T> typeInfo, string operation)
    {
        string payload = CallRawNoHandle(nativeCall, operation);
        ThrowIfRustError(payload, operation);

        T? response = JsonSerializer.Deserialize(payload, typeInfo);
        if (response is null)
        {
            throw new InvalidOperationException("Rust core returned an empty JSON payload.");
        }

        return response;
    }

    /// <summary>A handle-less call that returns nothing: a Rust error envelope becomes an exception.</summary>
    private void CallNoHandle(Func<IntPtr> nativeCall, string operation)
        => ThrowIfRustError(CallRawNoHandle(nativeCall, operation), operation);

    private string CallRawNoHandle(Func<IntPtr> nativeCall, string operation)
    {
        try
        {
            return ReadUtf8Response(nativeCall(), operation);
        }
        catch (Exception ex)
        {
            string diagnosticPath = WriteDiagnostic(operation, ex, null);
            throw new InvalidOperationException(BuildUserMessage(ex, diagnosticPath), ex);
        }
    }

    private static void ThrowIfRustError(string payload, string operation)
    {
        try
        {
            using JsonDocument document = JsonDocument.Parse(payload);
            if (document.RootElement.ValueKind is not JsonValueKind.Object)
            {
                return;
            }

            if (!document.RootElement.TryGetProperty("ok", out JsonElement okElement) ||
                okElement.ValueKind is not JsonValueKind.False)
            {
                return;
            }

            ErrorEnvelope? error = JsonSerializer.Deserialize(payload, OmniJsonSerializerContext.Default.ErrorEnvelope);
            if (error?.Error is not null)
            {
                throw new InvalidOperationException(MapRustErrorToRussian(error.Error, operation));
            }
        }
        catch (JsonException)
        {
            // Successful payloads are often typed snapshots, not error envelopes.
        }
    }

    private string CallRaw(Func<IntPtr> nativeCall, string operation)
    {
        lock (syncRoot)
        {
            EnsureInitialized();

            try
            {
                long started = BridgeCost.Start();
                string response = ReadUtf8Response(nativeCall(), operation);
                BridgeCost.Record(operation, started, response.Length);
                return response;
            }
            catch (Exception ex)
            {
                string diagnosticPath = WriteDiagnostic(operation, ex, null);
                throw new InvalidOperationException(BuildUserMessage(ex, diagnosticPath), ex);
            }
        }
    }

    /// <summary>
    /// A call whose slow part (git) the core already runs outside its own serializer: it does not take
    /// <see cref="syncRoot"/> either, so the shell's other calls (switching sections) do not wait for it.
    /// </summary>
    private string CallRawLong(Func<IntPtr> nativeCall, string operation)
    {
        handleLifetime.EnterReadLock();
        try
        {
            EnsureInitialized();
            try
            {
                long started = BridgeCost.Start();
                string response = ReadUtf8Response(nativeCall(), operation);
                BridgeCost.Record(operation, started, response.Length);
                return response;
            }
            catch (Exception ex)
            {
                string diagnosticPath = WriteDiagnostic(operation, ex, null);
                throw new InvalidOperationException(BuildUserMessage(ex, diagnosticPath), ex);
            }
        }
        finally
        {
            handleLifetime.ExitReadLock();
        }
    }

    private T CallJsonLong<T>(Func<IntPtr> nativeCall, JsonTypeInfo<T> typeInfo, string operation)
    {
        string payload = CallRawLong(nativeCall, operation);
        ThrowIfRustError(payload, operation);
        return JsonSerializer.Deserialize(payload, typeInfo)
            ?? throw new InvalidOperationException("Rust core returned an empty JSON payload.");
    }

    private T? CallOptionalJsonLong<T>(Func<IntPtr> nativeCall, JsonTypeInfo<T> typeInfo, string operation)
        where T : class
    {
        string payload = CallRawLong(nativeCall, operation);
        ThrowIfRustError(payload, operation);
        return string.Equals(payload.Trim(), "null", StringComparison.Ordinal) ? null : JsonSerializer.Deserialize(payload, typeInfo);
    }

    private static string ReadUtf8Response(IntPtr responsePtr, string operation)
    {
        if (responsePtr == IntPtr.Zero)
        {
            throw new InvalidOperationException($"Ядро Omni не ответило ({operation}).");
        }

        try
        {
            return Marshal.PtrToStringUTF8(responsePtr)
                ?? throw new InvalidOperationException("Ядро Omni ответило нечитаемо.");
        }
        finally
        {
            NativeMethods.omni_string_free(responsePtr);
        }
    }

    private void EnsureInitialized()
    {
        if (handle == IntPtr.Zero)
        {
            throw new InvalidOperationException("Omni ещё запускается — повторите через несколько секунд.");
        }
    }

    private static string MapRustErrorToRussian(RustError error, string operation)
    {
        if (TryMapProfileLifecycleError(error.Message, out string? lifecycleMessage))
        {
            return lifecycleMessage!;
        }

        return error.Kind switch
        {
            "unsupported_legacy_local_data" => "Эта локальная база данных создана старой неподдерживаемой архитектурой. Удалите локальные данные и начните заново либо восстановитесь по фразе.",
            "unsupported_database_format" => "Формат локальной базы данных больше не поддерживается. Удалите локальные данные и начните заново либо восстановитесь по фразе.",
            "unsupported_legacy_profile_state" => "Состояние локального профиля относится к старой неподдерживаемой архитектуре. Удалите локальные данные и начните заново либо восстановитесь по фразе.",
            "storage_locked" => "Данные зашифрованы ключом другого компьютера. Откройте их фразой восстановления одного из профилей.",
            "invalid_input" => operation switch
            {
                "unlock_database_with_mnemonic" when error.Message.Contains("not one of this database", StringComparison.OrdinalIgnoreCase)
                    => "Эта фраза — не от профиля из этих данных. Введите фразу одного из профилей, которые были на том компьютере.",
                "unlock_database_with_mnemonic" when error.Message.Contains("does not open this database", StringComparison.OrdinalIgnoreCase)
                    => "Фраза не открывает эти данные — проверьте слова и их порядок.",
                "unlock_database_with_mnemonic" when error.Message.Contains("must not be empty", StringComparison.OrdinalIgnoreCase)
                    => "Введите фразу восстановления.",
                // A recovery phrase that does not parse (restore, backup import, unlock by phrase).
                _ when Presentation.RecoveryText.ForCoreError(error.Message) is { } phraseText => phraseText,
                // Change / remove / key move with a wrong current password (QA 2026-10-17: it said
                // «Некорректные входные данные»).
                _ when error.Message.Contains("password does not unlock", StringComparison.OrdinalIgnoreCase)
                    => "Неверный текущий пароль.",
                // TM-006 confinement (save, export, delete): it said «Некорректные входные данные».
                _ when error.Message.Contains("protected system directory", StringComparison.OrdinalIgnoreCase)
                    => "В системную папку Windows сохранять нельзя — выберите другую, например «Загрузки».",
                // The rest of the same check: no such drive or folder, not an absolute path.
                _ when error.Message.Contains("destructive path", StringComparison.OrdinalIgnoreCase)
                    => "Эта папка недоступна — нет такого диска или папки. Выберите другую, например «Загрузки».",
                // Before the "body" rule: an over-long message said «Введите сообщение» (QA 2026-10-17).
                "send_text" when error.Message.Contains("byte limit", StringComparison.OrdinalIgnoreCase)
                    => Presentation.GroupErrorText.TooLongMessage,
                "send_text" when error.Message.Contains("body", StringComparison.OrdinalIgnoreCase) => "Введите сообщение.",
                "send_text" => "Укажите узел для отправки сообщения.",
                "list_messages" => "Укажите узел для загрузки истории.",
                "identity_create_account" => "Укажите имя профиля и название устройства.",
                "identity_restore_account_from_export_manifest" => "Проверьте файл экспорта и фразу восстановления.",
                "identity_restore_account_from_input" => "Проверьте фразу восстановления и название устройства.",
                "identity_export_current_database_backup" => "Не удалось создать резервную копию базы данных.",
                "identity_inspect_account_backup_manifest" => "Проверьте файл резервной копии и фразу восстановления.",
                "contacts_add_from_payload" => "Не удалось импортировать карточку контакта.",
                "contacts_rename" => "Укажите новое имя контакта.",
                "contacts_block" or "contacts_get_card" or "contacts_get_verification_state" or "contacts_confirm_verification"
                    => error.Message is null or "" ? "Не удалось обработать данные контакта." : $"Не удалось обработать данные контакта: {error.Message}",
                // Group actions: the core says exactly why (rights, membership) — keep it.
                _ when operation.StartsWith("groups_", StringComparison.Ordinal)
                    => Presentation.GroupErrorText.ForCoreMessage(error.Message),
                // Workspace actions: the core says which folder, version or role — keep it.
                _ when operation.StartsWith("workspace_", StringComparison.Ordinal)
                    => Presentation.WorkspaceErrorText.ForCoreMessage(error.Message),
                _ => "Некорректные входные данные.",
            },
            // Принять звонок можно только если согласие ДОШЛО. Ядро отказывает, когда живого пути
            // нет и конверт лёг бы в durable «доставлю позже» — для звонка это не доставка.
            // Звонок при этом остаётся звонящим, поэтому в тексте есть что делать дальше.
            "transport" when operation == "calls_accept_call" =>
                "Собеседник сейчас недоступен — согласие на звонок не доставлено. Звонок продолжает звонить, попробуйте ещё раз.",
            "transport" when operation == "send_text" => error.Message is null or ""
                ? "Ошибка отправки."
                : $"Ошибка отправки: {error.Message}",
            "transport" when operation.StartsWith("discovery", StringComparison.OrdinalIgnoreCase) => error.Message is null or ""
                ? "Не удалось обновить поиск узлов в локальной сети."
                : $"Не удалось обновить поиск узлов в локальной сети: {error.Message}",
            // A file the user picked the place for (a backup, a download): the place is the likely
            // reason — no such drive, no right to write there. It said only «Ошибка локального
            // хранилища» (QA 2026-10-17).
            "storage" when operation is "identity_export_current_database_backup_bundle" or "export_database_backup_bundle"
                => "Не удалось записать копию в эту папку — нет такого диска или нет прав на запись. Выберите другую, например «Документы».",
            "storage" => "Ошибка локального хранилища.",
            _ => error.Message,
        };
    }

    private static bool TryMapProfileLifecycleError(string message, out string? mappedMessage)
    {
        mappedMessage = null;
        const string Marker = "profile_lifecycle:";
        if (!message.StartsWith(Marker, StringComparison.Ordinal))
        {
            return false;
        }

        string[] parts = message.Split(':', 3, StringSplitOptions.None);
        if (parts.Length < 3)
        {
            return false;
        }

        mappedMessage = parts[1] switch
        {
            "no_active_profile" => "Активный профиль не выбран.",
            "active_profile_switch_required" => "Сначала выберите активный профиль в списке.",
            "profile_locked" => "Профиль заблокирован. Разблокируйте его паролем или фразой восстановления.",
            "profile_deleting" => "Профиль сейчас удаляется. Обычные действия временно недоступны.",
            "profile_deleted" => "Профиль уже удалён с этого устройства.",
            "profile_already_present" => "Этот локальный профиль уже существует на устройстве. Откройте его из списка профилей вместо восстановления по фразе.",
            "lifecycle_transition_not_allowed" => "Это действие недоступно для текущего состояния профиля.",
            "unlock_failed_invalid_password" => "Неверный пароль разблокировки.",
            "unlock_failed_invalid_mnemonic" => "Фраза восстановления не подходит для этого профиля.",
            "dangerous_action_confirmation_required" => "Нужно ввести корректный код подтверждения опасного действия.",
            "runtime_context_unavailable" => "Контекст профиля сейчас недоступен.",
            _ => parts[2],
        };
        return true;
    }

    private static string BuildUserMessage(Exception ex, string diagnosticPath)
    {
        string detailsHint = $" Подробности для разработки: {diagnosticPath}";

        return ex switch
        {
            NativeLibraryLoadException nativeLoadException => nativeLoadException.Message + detailsHint,
            BadImageFormatException => "Эта сборка Omni не подходит к компьютеру: нужна версия для другой архитектуры." + detailsHint,
            DllNotFoundException => "Не найден файл omni_ffi.dll или одна из его зависимостей." + detailsHint,
            _ => ex.Message,
        };
    }

    private string WriteDiagnostic(string operation, Exception ex, string? databasePath)
    {
        string logsDirectory = Path.Combine(appDirectory, "logs");
        Directory.CreateDirectory(logsDirectory);

        string logPath = Path.Combine(logsDirectory, "app-runtime.log");
        List<string> lines =
        [
            $"timestamp={DateTimeOffset.Now:O}",
            $"operation={operation}",
            $"base_directory={AppContext.BaseDirectory}",
            $"process_path={Environment.ProcessPath}",
            $"process_architecture={RuntimeInformation.ProcessArchitecture}",
            $"os_architecture={RuntimeInformation.OSArchitecture}",
            $"loaded_library_path={NativeMethods.LoadedLibraryPath ?? "<not loaded>"}",
            $"database_path={databasePath ?? "<not_applicable>"}",
            $"exception_type={ex.GetType().FullName}",
            $"exception_message={ex.Message}",
            "native_diagnostic_begin",
            ex is NativeLibraryLoadException nativeLoadException
                ? nativeLoadException.DiagnosticDetails
                : NativeMethods.LastLoadDiagnostic,
            "native_diagnostic_end",
        ];

        if (ex.InnerException is not null)
        {
            lines.Add($"inner_exception_type={ex.InnerException.GetType().FullName}");
            lines.Add($"inner_exception_message={ex.InnerException.Message}");
        }

        File.WriteAllText(logPath, string.Join(Environment.NewLine, lines) + Environment.NewLine);
        Debug.WriteLine(string.Join(Environment.NewLine, lines));

        return logPath;
    }

    /// <summary>
    /// Одна папка данных — одно окно. Два экземпляра на одной базе не «медленно работают», а
    /// намертво замирают ОБА: они дерутся за одну SQLite, и пользователь получает два окна, ни одно
    /// из которых не отвечает, без единого объяснения (поймано живьём 2026-08-14 — вторым запуском
    /// той же задачи планировщика).
    ///
    /// Несколько окон продуктом ЗАДУМАНЫ — интерфейс так и говорит, «локальные данные этого окна», —
    /// но у каждого должна быть своя папка (`workspaces/<id>`). Без явного id оба экземпляра берут
    /// одну и ту же папку по умолчанию, и защиты не было никакой.
    ///
    /// Замок — файл, открытый монопольно: он снимается вместе с процессом, в том числе убитым, и
    /// потому не может «залипнуть» после падения, как залипла бы запись в базе.
    /// </summary>
    private void AcquireAppDirectoryLock()
    {
        if (appDirectoryLock is not null)
        {
            return;
        }

        string lockPath = Path.Combine(appDirectory, "omni.window.lock");
        try
        {
            appDirectoryLock = new FileStream(
                lockPath,
                FileMode.OpenOrCreate,
                FileAccess.ReadWrite,
                FileShare.None,
                bufferSize: 1,
                FileOptions.DeleteOnClose);
        }
        catch (IOException)
        {
            throw new InvalidOperationException(
                $"Эти данные уже открыты в другом окне Omni ({appDirectory}). "
                + "Закройте то окно или запустите второе с собственной папкой данных.");
        }
    }

    private void ReleaseAppDirectoryLock()
    {
        appDirectoryLock?.Dispose();
        appDirectoryLock = null;
    }

    private void InitCore()
    {
        if (handle != IntPtr.Zero)
        {
            return;
        }

        Directory.CreateDirectory(appDirectory);
        AcquireAppDirectoryLock();

        string databasePath = GetDatabasePath();
        DatabaseBootstrapSummary bootstrapSummary = InspectDatabaseBootstrap();
        if (!bootstrapSummary.Supported)
        {
            throw new InvalidOperationException(
                bootstrapSummary.UnsupportedReason
                ?? "Старая локальная база данных не поддерживается. Удалите локальные данные и начните заново либо восстановитесь по фразе.");
        }

        string configJson = JsonSerializer.Serialize(
            new InitConfigRequest(databasePath),
            OmniJsonSerializerContext.Default.InitConfigRequest);

        try
        {
            NativeMethods.EnsureLoaded();
            handle = NativeMethods.omni_app_init(configJson);

            if (handle == IntPtr.Zero)
            {
                // Показываем ПРИЧИНУ. Без неё это сообщение бесполезно: живой случай 2026-08-14 —
                // профиль перестал открываться, и настоящую причину (исчерпанный пул одноразовых
                // предключей) можно было узнать только из файла трассы.
                string reason = NativeMethods.omni_app_last_init_error();
                throw new InvalidOperationException(string.IsNullOrWhiteSpace(reason)
                    ? "Ядро Omni не запустилось."
                    : $"Ядро Omni не запустилось: {reason}");
            }

#if OMNI_DEV_TOOLS
            // A command pipe that drives the live app is a dev/validation tool, not a product surface.
            StartEmbeddedDaemon();
#endif
        }
        catch (Exception ex)
        {
            FreeHandle();

            string diagnosticPath = WriteDiagnostic("init", ex, databasePath);
            throw new InvalidOperationException(BuildUserMessage(ex, diagnosticPath), ex);
        }
    }

    /// <summary>
    /// The named pipe the in-process embedded daemon listens on so an external client (a
    /// headless omni-node `--connect`, or an automation/dev tool) can drive THIS running core
    /// over the daemon protocol; the shell reflects the changes through its normal background
    /// poll. The pipe carries a restrictive DACL (current user only).
    /// See docs/decisions/embedded-daemon-in-shell.md.
    /// </summary>
    // Pipe the embedded daemon listens on. Overridable via OMNI_DAEMON_PIPE so several app instances
    // can run side by side (each its own pipe) for multi-window dev/testing — mirrors the
    // OMNI_APPDATA_DIR data-dir override. Default keeps the single-instance name.
    private static readonly string EmbeddedDaemonEndpoint =
        Environment.GetEnvironmentVariable("OMNI_DAEMON_PIPE") is { Length: > 0 } daemonPipe
            ? daemonPipe
            : @"\\.\pipe\omni-shell";

    /// <summary>
    /// Start the embedded daemon against the live core. Best-effort: a failure (e.g. another
    /// shell instance already owns the pipe) is logged, never fatal to startup.
    /// </summary>
    private void StartEmbeddedDaemon()
    {
        try
        {
            string payload = ReadUtf8Response(
                NativeMethods.omni_daemon_start(handle, EmbeddedDaemonEndpoint),
                "daemon_start");
            Debug.WriteLine(
                payload.Contains("\"ok\":true", StringComparison.Ordinal)
                    ? $"Embedded daemon listening on {EmbeddedDaemonEndpoint}"
                    : $"Embedded daemon did not start: {payload}");
        }
        catch (Exception ex)
        {
            Debug.WriteLine($"Embedded daemon start failed: {ex.Message}");
        }
    }

    private void DisposeCore()
    {
        if (handle == IntPtr.Zero)
        {
            // Замок берётся ДО инициализации ядра, поэтому снимать его надо и тогда, когда ядро так
            // и не поднялось — иначе неудачный старт оставил бы папку занятой до конца процесса.
            ReleaseAppDirectoryLock();
            return;
        }

        FreeHandle();
        ReleaseAppDirectoryLock();
        GC.SuppressFinalize(this);
    }

    /// <summary>Frees the core once no long call (<see cref="CallRawLong"/>) is inside it.</summary>
    private void FreeHandle()
    {
        handleLifetime.EnterWriteLock();
        try
        {
            if (handle != IntPtr.Zero)
            {
                NativeMethods.omni_app_free(handle);
                handle = IntPtr.Zero;
            }
        }
        finally
        {
            handleLifetime.ExitWriteLock();
        }
    }

    private string GetDatabasePath()
        => Path.Combine(appDirectory, "omni.db");

    internal sealed record InitConfigRequest([property: JsonPropertyName("db_path")] string DbPath);

    internal sealed record DatabaseLockStateResponse([property: JsonPropertyName("state")] string State);

    internal sealed record UnlockDatabaseWithMnemonicRequest([property: JsonPropertyName("mnemonic")] string Mnemonic);

    internal sealed record DatabaseBootstrapSummary(
        [property: JsonPropertyName("state")] string State,
        [property: JsonPropertyName("supported")] bool Supported,
        [property: JsonPropertyName("reset_required")] bool ResetRequired,
        [property: JsonPropertyName("can_create_new_account")] bool CanCreateNewAccount,
        [property: JsonPropertyName("can_restore_from_mnemonic")] bool CanRestoreFromMnemonic,
        [property: JsonPropertyName("unsupported_reason")] string? UnsupportedReason);

    internal sealed record ErrorEnvelope(
        [property: JsonPropertyName("ok")] bool Ok,
        [property: JsonPropertyName("error")] RustError? Error);

    internal sealed record RustError(
        [property: JsonPropertyName("kind")] string Kind,
        [property: JsonPropertyName("message")] string Message);
}
