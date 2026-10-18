// Приёмник обязан ОТВЕЧАТЬ на разрыв счётчика, а не только писать строчку в лог.
//
// docs/bugs/session-wedged-by-counter-gap.md — часть 2.
//
// Когда счётчик отправителя убегает за окно приёма, приёмник отвергает КАЖДЫЙ конверт, включая
// presence-пинги. Для отправителя это неотличимо от «пир офлайн»: он не получает ответа и ретраит.
// Сессия заклинивает намертво, и перезапуск не помогает — запись сессии персистентна. На живом
// замере так набралось 955 отказов подряд при идеальном транспорте.
//
// Часть 1 (кэш исходящих конвертов) не даёт разрыву РАСТИ, но уже разошедшиеся счётчики не сводит.
// Свести их может только пересборка сессии, а начать её должен ОТПРАВИТЕЛЬ: сессия выводится из его
// bootstrap'а. Значит приёмнику нужен способ сказать «пересобери».
//
// Способ берётся существующий: presence-пинг со специальным `ping_id`, ровно как уже сделаны
// сигналы запроса и одобрения контакта (`__omni_contact_request__` / `__omni_contact_approved__`).
// Ни нового кадра, ни нового пути — тот же конверт, тот же обработчик.
//
// Почему это вообще доезжает: счётчики направлений НЕЗАВИСИМЫ (`send_counter` и `receive_counter` —
// разные поля сессии). Заклинивает одно направление; обратное продолжает работать, и по нему
// сигнал уходит.

/// Тестовый шов: узел ПОЛУЧАЕТ зашифрованные конверты и молча их роняет — ровно так, как если бы их
/// потеряла сеть. Счётчик приёма стоит на месте, счётчик отправки на той стороне уходит вперёд, и
/// разрыв растёт: без этого шва воспроизвести заклинивание на исправной локальной сети нечем.
///
/// Роняем ДО расшифровки и только `EncryptedPayload`: bootstrap проходит как обычно, иначе пара
/// просто не спарится и стенд ничего не измерит. Для крипто-слоя это неотличимо от потери пакета —
/// шов не подменяет поведение, а воспроизводит условие.
///
/// Родня `OMNI_TEST_PRESENCE_DEAF` и `OMNI_MEDIA_TEST_PACKET_LOSS_PERCENT`: включается только
/// переменной окружения, в продуктовом запуске не существует.
fn envelope_deaf_for_tests() -> bool {
    // Fault injection is a test-stand knob: release builds ignore it unless built with `dev-tools`.
    if !cfg!(any(debug_assertions, feature = "dev-tools")) {
        return false;
    }
    static DEAF: OnceLock<bool> = OnceLock::new();
    *DEAF.get_or_init(|| {
        std::env::var("OMNI_TEST_ENVELOPE_DEAF")
            .map(|value| {
                let value = value.trim().to_ascii_lowercase();
                !value.is_empty() && value != "0" && value != "false"
            })
            .unwrap_or(false)
    })
}

/// Сколько подряд отвергнутых по разрыву счётчика конвертов от одного диалога считаем
/// доказательством рассинхронизации.
///
/// Одиночный разрыв — нормальное явление (конверт разъехался путями, пир перезапустился между
/// отправками). Три подряд уже означают, что окно вылетело и само не вернётся: отправитель шлёт, мы
/// отвергаем, ответить нам нечем.
const SESSION_RESYNC_GAP_THRESHOLD: u32 = 3;

/// Не чаще раза в это время на диалог. Пересборка сессии — не бесплатная операция, и просить её на
/// каждый отвергнутый конверт (а их приходят сотни) значило бы заменить один шторм другим.
const SESSION_RESYNC_MIN_INTERVAL_MS: i64 = 30_000;

/// Потолок пересборок за окно ниже. Нужен против ЗЛОГО пира, а не против сети.
///
/// Реакция на разрыв счётчика неизбежно срабатывает ДО проверки подлинности: чтобы проверить MAC,
/// надо сначала довести цепочку ключей до счётчика конверта, а именно этого мы и не можем сделать при
/// разрыве. Значит одобренный, но враждебный контакт способен вызывать пересборку, не подделав ни
/// одной подписи, — достаточно слать конверты с заведомо далёкими счётчиками.
///
/// Одной частоты мало: она размазывает нагрузку, но не ограничивает её общий объём. Поэтому пересборок
/// не больше `SESSION_REBUILD_BUDGET` за `SESSION_REBUILD_BUDGET_WINDOW_MS`; исчерпав бюджет,
/// перестаём пересобирать и пишем это в трассу. Настоящая рассинхронизация лечится с первой-второй
/// попытки, так что честному случаю бюджета хватает с запасом.
const SESSION_REBUILD_BUDGET: u32 = 5;
const SESSION_REBUILD_BUDGET_WINDOW_MS: i64 = 60 * 60_000;

/// Сколько подряд не прошедших аутентификацию кадров одобренного контакта считаем доказательством,
/// что сессия разошлась (стороны вывели разные ключи — docs/bugs/repair-on-restart-diverges-session.md).
///
/// Выше порога разрыва: одиночные провалы бывают и у живой сессии (кадр старой сессии доехал после
/// пересборки). Разошедшаяся же не расшифровывает НИЧЕГО — presence-пробы идут каждые несколько
/// секунд, и порог набирается за полминуты.
const SESSION_RESYNC_AUTH_FAILURE_THRESHOLD: u32 = 5;

/// Сколько после создания сессии провалы аутентификации не считаются: это кадры прошлой сессии,
/// которые ещё едут или лежат у пира в durable. Без окна пересборка вызывала бы следующую.
const SESSION_RESYNC_FRESH_SESSION_GRACE_MS: i64 = 30_000;

#[derive(Debug, Clone, Default)]
struct SessionResyncEntry {
    consecutive_gap_errors: u32,
    last_requested_at_unix_ms: Option<i64>,
    rebuilds_in_window: u32,
    budget_window_started_at_unix_ms: Option<i64>,
}

static SESSION_RESYNC: OnceLock<
    Mutex<std::collections::HashMap<String, std::collections::HashMap<String, SessionResyncEntry>>>,
> = OnceLock::new();

fn session_resync_registry() -> &'static Mutex<
    std::collections::HashMap<String, std::collections::HashMap<String, SessionResyncEntry>>,
> {
    SESSION_RESYNC.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Конверт принят — счётчик подряд идущих разрывов сбрасывается.
fn note_session_payload_accepted(db_path: &str, dialog_key: &str) {
    let dialog_key = normalize_dialog_key(dialog_key);
    if let Ok(mut registry) = session_resync_registry().lock() {
        if let Some(profile) = registry.get_mut(db_path) {
            if let Some(entry) = profile.get_mut(&dialog_key) {
                entry.consecutive_gap_errors = 0;
            }
        }
    }
}

/// Конверт отвергнут по разрыву счётчика. Возвращает `true`, когда пора просить пересборку:
/// накопился порог И прошло достаточно времени с прошлой просьбы.
fn note_session_counter_gap(db_path: &str, dialog_key: &str, now_unix_ms: i64) -> bool {
    note_session_rejection(db_path, dialog_key, now_unix_ms, SESSION_RESYNC_GAP_THRESHOLD)
}

/// Кадр не прошёл аутентификацию. Те же частота и бюджет, что у разрыва (счёт подряд общий — любой
/// принятый кадр его обнуляет), но порог выше, а у свежей сессии провалы не считаются вовсе.
fn note_session_auth_failure(
    db_path: &str,
    dialog_key: &str,
    now_unix_ms: i64,
    session_created_at_unix_ms: i64,
) -> bool {
    if now_unix_ms.saturating_sub(session_created_at_unix_ms) < SESSION_RESYNC_FRESH_SESSION_GRACE_MS {
        return false;
    }
    note_session_rejection(db_path, dialog_key, now_unix_ms, SESSION_RESYNC_AUTH_FAILURE_THRESHOLD)
}

fn note_session_rejection(db_path: &str, dialog_key: &str, now_unix_ms: i64, threshold: u32) -> bool {
    let dialog_key = normalize_dialog_key(dialog_key);
    let Ok(mut registry) = session_resync_registry().lock() else {
        return false;
    };
    let entry = registry
        .entry(db_path.to_string())
        .or_default()
        .entry(dialog_key)
        .or_default();
    entry.consecutive_gap_errors = entry.consecutive_gap_errors.saturating_add(1);
    if entry.consecutive_gap_errors < threshold {
        return false;
    }
    let too_soon = entry
        .last_requested_at_unix_ms
        .is_some_and(|at| now_unix_ms.saturating_sub(at) < SESSION_RESYNC_MIN_INTERVAL_MS);
    if too_soon {
        return false;
    }
    // Бюджет — против злого пира: частота размазывает нагрузку, но не ограничивает её общий объём.
    let window_expired = entry
        .budget_window_started_at_unix_ms
        .is_none_or(|at| now_unix_ms.saturating_sub(at) >= SESSION_REBUILD_BUDGET_WINDOW_MS);
    if window_expired {
        entry.budget_window_started_at_unix_ms = Some(now_unix_ms);
        entry.rebuilds_in_window = 0;
    }
    if entry.rebuilds_in_window >= SESSION_REBUILD_BUDGET {
        omni_app_trace_log(
            "session/rebuild",
            &format!(
                "skip=budget_exhausted rebuilds={} window_ms={SESSION_REBUILD_BUDGET_WINDOW_MS}",
                entry.rebuilds_in_window
            ),
        );
        return false;
    }
    entry.rebuilds_in_window = entry.rebuilds_in_window.saturating_add(1);
    entry.last_requested_at_unix_ms = Some(now_unix_ms);
    true
}

fn clear_session_resync_for_profile(db_path: &str) {
    if let Ok(mut registry) = session_resync_registry().lock() {
        registry.remove(db_path);
    }
}
