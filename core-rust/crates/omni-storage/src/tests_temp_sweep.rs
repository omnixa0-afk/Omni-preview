// Тестовые базы живут в системном temp и НИКОГДА не удалялись: каждый прогон оставлял свои файлы
// навсегда. К 2026-08-14 на рабочей машине накопилось 13.5 ГБ в 49 каталогах (24 605 файлов в одном
// только `omni-stage-5-app-tests`) — диск заполнился на 100%.
//
// Убирать за собой в конце теста нечем: у libtest нет общего teardown, а обвешивать сотни тестов
// RAII-сторожами — значит трогать каждый тест ради уборки. Поэтому подметаем на ВХОДЕ: при первом
// создании тестового пути в процессе сносим то, что заведомо осталось от прошлых прогонов.
//
// Метём весь корень сразу, а не по одному пространству имён: имён у тестов десятки, они заводятся
// по ходу дела, и уборка, о которой надо помнить при каждом новом имени, снова зарастёт.
//
// Метём ПРОГОНЫ, а не пространства имён. Первая версия смотрела на время самого `omni-stage-5-app-tests`
// — а его обновляет каждый новый прогон, создавая в нём свою базу, так что при прогонах чаще раза в
// два часа каталог не устаревал никогда и рос без конца: 2026-10-10 диск C: снова кончился (0 байт).
// Теперь возраст считается у каждой базы/каталога внутри, а опустевшее пространство имён уходит само.
//
// Одна реализация на omni-storage, omni-app и omni-ffi (раньше копия в каждом, а в ffi — никакой).
//
// Порог — 2 часа. Полный прогон занимает ~5 минут, так что ни свои файлы, ни файлы параллельного
// прогона не заденем при любом разумном стечении обстоятельств.

const TEST_TEMP_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(2 * 60 * 60);
const TEST_TEMP_PREFIX: &str = "omni-";

/// Снести то, что оставили прошлые прогоны тестов в системном temp. Один раз на процесс, только
/// лучшая попытка: уборка не имеет права ронять тест. Для тестов крейтов ядра, не для приложения.
#[doc(hidden)]
pub fn sweep_stale_test_temp() {
    static SWEPT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    SWEPT.get_or_init(|| {
        sweep_stale_test_temp_in(&std::env::temp_dir(), std::time::SystemTime::now());
    });
}

fn sweep_stale_test_temp_in(root: &std::path::Path, now: std::time::SystemTime) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(TEST_TEMP_PREFIX) {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            remove_test_temp_if_stale(&path, now);
            continue;
        }
        // An app's own folder under temp could also start with "omni-": only what is old goes.
        if let Ok(runs) = std::fs::read_dir(&path) {
            for run in runs.flatten() {
                remove_test_temp_if_stale(&run.path(), now);
            }
        }
        // Empty once its runs are gone (fails, harmlessly, while anything is left in it).
        let _ = std::fs::remove_dir(&path);
    }
}

fn remove_test_temp_if_stale(path: &std::path::Path, now: std::time::SystemTime) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    let stale = metadata
        .modified()
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age >= TEST_TEMP_STALE_AFTER);
    if !stale {
        return;
    }
    if metadata.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod test_temp_sweep_tests {
    use super::*;

    /// A namespace a fresh run just wrote into still loses the runs that are old.
    #[test]
    fn old_runs_go_even_from_a_namespace_a_new_run_just_wrote_to() {
        let root = std::env::temp_dir().join(format!("sweep-root-{}", std::process::id()));
        let namespace = root.join("omni-stage-5-app-tests");
        std::fs::create_dir_all(namespace.join("old-run")).unwrap();
        std::fs::write(namespace.join("old-run").join("omni.db"), b"x").unwrap();
        std::fs::write(namespace.join("old.db"), b"x").unwrap();
        // "Now" is three hours on, so everything above is old — then a fresh run writes its base.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3 * 60 * 60);
        std::fs::write(namespace.join("fresh.db"), b"x").unwrap();
        let fresh = std::fs::File::options().write(true).open(namespace.join("fresh.db")).unwrap();
        fresh.set_modified(later).unwrap();
        drop(fresh);

        sweep_stale_test_temp_in(&root, later);

        assert!(!namespace.join("old-run").exists(), "an old run's folder goes");
        assert!(!namespace.join("old.db").exists(), "an old run's base goes");
        assert!(namespace.join("fresh.db").exists(), "the fresh run's base stays");

        std::fs::remove_file(namespace.join("fresh.db")).unwrap();
        sweep_stale_test_temp_in(&root, later);
        assert!(!namespace.exists(), "an emptied namespace goes too");
        let _ = std::fs::remove_dir_all(&root);
    }
}
