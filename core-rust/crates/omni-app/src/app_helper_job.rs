// Дети-хелперы обязаны умирать вместе с нами — даже когда нас никто не спрашивал.
//
// `Drop` на это не годится, и это не вопрос аккуратности. Хелперы принадлежат статическому реестру,
// а статики в Rust при завершении процесса НЕ дропаются; плюс у оболочки есть жёсткий backstop,
// который добивает процесс, если окно не закрылось (`MainWindow.xaml.cs`, ShutdownExitGraceMs).
// Любой из этих путей оставлял живого курьера: наблюдалось 2026-08-13 — приложение закрыто, а
// `Omni.Windows.App.exe --run-helper rendezvous` держит exe и продолжает публиковать маяки на
// инстанс, которого больше нет (docs/bugs/rendezvous-helper-outlives-the-app.md). Мы же потом по
// этим маякам разбираем `NoPeerBeacon`.
//
// Windows умеет это гарантировать сам: job object с `KILL_ON_JOB_CLOSE`. Хэндл джоба живёт столько
// же, сколько процесс; когда процесс исчезает по ЛЮБОЙ причине — штатный выход, `Environment.Exit`,
// падение, `taskkill` — ядро закрывает последний хэндл и убивает всех, кто в джобе.
//
// `Drop` у хелперов остаётся: он честно закрывает ребёнка в обычной жизни (смена профиля, повторный
// запуск), а джоб — страховка на случаи, когда до `Drop` дело не доходит.
//
// На не-Windows это no-op: fail-open осознанный, там аналог (`prctl(PR_SET_PDEATHSIG)` / process
// groups) ставится иначе и появится вместе с портом. См. CONTRIBUTING.md, Android readiness.

#[cfg(windows)]
mod imp {
    use std::os::windows::io::AsRawHandle;
    use std::sync::OnceLock;

    type Handle = *mut core::ffi::c_void;

    #[link(name = "kernel32")]
    extern "system" {
        #[link_name = "CreateJobObjectW"]
        fn create_job_object(attributes: *mut core::ffi::c_void, name: *const u16) -> Handle;
        #[link_name = "SetInformationJobObject"]
        fn set_information_job_object(
            job: Handle,
            info_class: i32,
            info: *const core::ffi::c_void,
            info_len: u32,
        ) -> i32;
        #[link_name = "AssignProcessToJobObject"]
        fn assign_process_to_job_object(job: Handle, process: Handle) -> i32;
    }

    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;

    #[repr(C)]
    #[derive(Default)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct BasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimitInformation {
        basic_limit_information: BasicLimitInformation,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    /// Хэндл джоба намеренно не закрывается никогда: его закрытие ядром при смерти процесса — и есть
    /// механизм. Обёртка нужна только чтобы протащить сырой указатель через `OnceLock`.
    struct JobHandle(Handle);
    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}

    static JOB: OnceLock<Option<JobHandle>> = OnceLock::new();

    fn job() -> Option<Handle> {
        JOB.get_or_init(|| {
            // SAFETY: безымянный джоб с атрибутами по умолчанию; при неудаче возвращается null и
            // мы просто остаёмся без страховки.
            let handle = unsafe { create_job_object(std::ptr::null_mut(), std::ptr::null()) };
            if handle.is_null() {
                return None;
            }
            let mut info = ExtendedLimitInformation::default();
            info.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `info` живёт до конца вызова, размер передаётся точный.
            let ok = unsafe {
                set_information_job_object(
                    handle,
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                    (&info as *const ExtendedLimitInformation).cast(),
                    std::mem::size_of::<ExtendedLimitInformation>() as u32,
                )
            };
            if ok == 0 {
                // Без KILL_ON_JOB_CLOSE джоб бесполезен, а хуже того — вводил бы в заблуждение.
                return None;
            }
            Some(JobHandle(handle))
        })
        .as_ref()
        .map(|handle| handle.0)
    }

    /// Приписать ребёнка к джобу. Возвращает `false`, если страховки не получилось — вызывающий
    /// это только логирует: ребёнок всё равно запущен и `Drop` по-прежнему работает.
    pub(crate) fn adopt(child: &std::process::Child) -> bool {
        let Some(job) = job() else {
            return false;
        };
        // SAFETY: хэндл процесса валиден, пока жив `Child`.
        unsafe { assign_process_to_job_object(job, child.as_raw_handle().cast()) != 0 }
    }
}

#[cfg(not(windows))]
mod imp {
    pub(crate) fn adopt(_child: &std::process::Child) -> bool {
        false
    }
}

/// Привязать процесс-хелпер к времени жизни нашего процесса. Идемпотентно и безопасно вызывать
/// на каждый спавн.
pub(crate) fn adopt_helper_child(child: &std::process::Child, what: &str) {
    let adopted = imp::adopt(child);
    if !adopted {
        crate::omni_app_trace_log(
            "helper/job",
            &format!("what={what} adopted=false (helper may outlive the app)"),
        );
    }
}
