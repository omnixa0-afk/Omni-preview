// Достижимость контакта — свойство omni session, а не несущей.
// docs/decisions/reachability-belongs-to-the-session.md, ШАГ 1.
//
// Сессия уже знает, сколько мы отправили (`send_counter`) и сколько проверенно приняли
// (`receive_counter`). Чего она не знала — что из отправленного пир ПОДТВЕРДИЛ. Именно этого факта
// не хватает, чтобы отличить «соединение открыто» от «меня слышат»: живой замер на мобильном
// (docs/bugs/mesh-link-flaps-on-mobile.md) дал 496 успешных `carrier/out outcome=sent` при нулевом
// входящем — транспорт жив, приложение на той стороне не получает ничего.
//
// Здесь этот факт заводится и только записывается. Вердикта (шаг 2) и его потребителей (шаг 3)
// здесь нет намеренно: шаг 1 не должен менять поведение, чтобы его можно было выкатить и сверить
// с реальностью на живом канале.
//
// Почему НЕ поле в персистентном `SessionRecord`:
//  * запись сессии живёт циклом read-modify-write (`prepare_encrypted_payload_for_established_session`
//    → `store_session_record`), и подтверждение, положенное туда, затирается любой параллельной
//    отправкой, которая держит более старый клон записи;
//  * это факт живости, а не крипто-состояние: пережив перезапуск процесса, он бы утверждал
//    достижимость через транспорт, которого больше нет. Честный ответ после старта — «не знаю».
//
// Почему счётчик отправки не годится как «подтверждено вплоть до N»: `send_counter` обнуляется на
// каждом шаге DH-ratchet (`internals_ratchet.rs`), то есть монотонным по сессии он не является.
// Носитель смысла здесь — ВРЕМЯ доказанного круга (и возраст самой старой неотвеченной пробы),
// а не номер.

/// Тестовый шов: узел ПРИНИМАЕТ presence-пинги, но не отвечает на них. Воспроизводит на исправном
/// канале ровно тот класс отказа, ради которого всё это заведено, — транспорт жив, приложение на
/// той стороне глухо (`docs/bugs/mesh-link-flaps-on-mobile.md`). Живость самой несущей это НЕ
/// трогает: mesh-овые `Ping`/`Pong` — другой механизм, линк держится.
///
/// Глушатся только keepalive-пинги. Сигналы запроса и одобрения контакта отвечаются как обычно,
/// иначе узел с этим флагом просто не спарится и стенд ничего не измерит.
///
/// Аналог `OMNI_MEDIA_TEST_PACKET_LOSS_PERCENT` в медиа-слое: включается только переменной
/// окружения, в продуктовом запуске не существует.
fn presence_deaf_for_tests() -> bool {
    // Fault injection is a test-stand knob: release builds ignore it unless built with `dev-tools`.
    if !cfg!(any(debug_assertions, feature = "dev-tools")) {
        return false;
    }
    static DEAF: OnceLock<bool> = OnceLock::new();
    *DEAF.get_or_init(|| {
        std::env::var("OMNI_TEST_PRESENCE_DEAF")
            .map(|value| {
                let value = value.trim().to_ascii_lowercase();
                !value.is_empty() && value != "0" && value != "false"
            })
            .unwrap_or(false)
    })
}

/// Одна наша проба (presence-ping), на которую ещё не пришёл ответ.
#[derive(Debug, Clone)]
struct SessionProbe {
    ping_id: String,
    sent_at_unix_ms: i64,
}

#[derive(Debug, Clone, Default)]
struct SessionReachabilityEntry {
    unanswered: std::collections::VecDeque<SessionProbe>,
    acknowledged_at_unix_ms: Option<i64>,
    round_trip_ms: Option<i64>,
    probes_sent: u64,
    probes_answered: u64,
    /// Когда мы в последний раз ПЫТАЛИСЬ чинить путь к этому пиру (сбрасывали фантомный линк).
    /// Нужен, чтобы восстановление не превратилось в машину churn'а: у очереди свой такт 250 мс,
    /// и без этой отметки снос повторялся бы четыре раза в секунду.
    recovery_attempted_at_unix_ms: Option<i64>,
    /// Ответы, для которых своей пробы уже не нашлось (вытеснена лимитом или просрочена, либо это
    /// pong на bootstrap-сигнал, который мы пробой не регистрируем). Круг они всё равно доказывают,
    /// но RTT по ним не считается — счётчик нужен, чтобы на живой сверке (шаг 2) было видно, как
    /// часто так происходит.
    uncorrelated_answers: u64,
    /// Когда наш исходящий поток к пиру (передача файла) в последний раз продвинулся. Запись в поток
    /// QUIC стоит на обратном давлении получателя, поэтому продвижение — тоже доказательство, что
    /// пир нас слышит. Нужно оттого, что под мегабайтами файла пробы присутствия стоят в очереди за
    /// данными по 10 с и дольше, и «неотвеченные пробы» сносили живую связь посреди передачи
    /// (живьём 2026-10-10: 100 МБ ноутбук → ПК оборвались на 7-м мегабайте).
    outbound_progress_at_unix_ms: Option<i64>,
}

/// Снимок того, что сессия знает о своей достижимости. Только чтение; решений не принимает.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionAcknowledgement {
    /// Когда пир в последний раз ДОКАЗАЛ, что слышит нас (пришёл pong на наш ping).
    /// `None` — доказательств не было ни разу с момента старта.
    pub(crate) acknowledged_at_unix_ms: Option<i64>,
    pub(crate) round_trip_ms: Option<i64>,
    pub(crate) unanswered_probes: usize,
    pub(crate) oldest_unanswered_probe_at_unix_ms: Option<i64>,
    pub(crate) probes_sent: u64,
    pub(crate) probes_answered: u64,
    pub(crate) uncorrelated_answers: u64,
}

/// Сколько неотвеченных проб держим на сессию. Больше не нужно: для вердикта важна самая старая,
/// а на живом канале очередь и так не растёт.
const SESSION_PROBE_TRACKING_LIMIT: usize = 8;

/// Проба старше этого ничего уже не доказывает, даже если ответ на неё придёт: круг такой длины
/// не отличим от «канал восстановился только что».
const SESSION_PROBE_EXPIRY_MS: i64 = 120_000;

#[derive(Debug, Clone, Default)]
struct SessionReachabilityProfile {
    sessions: std::collections::HashMap<String, SessionReachabilityEntry>,
    /// `dialog_key` → ключ сессии. Несущая знает только диалог, а факт живёт у сессии; карта
    /// заполняется в тех же двух точках, где обе величины и так на руках (отправка пробы и разбор
    /// pong'а), поэтому спрашивать хранилище на горячем пути отправки не нужно.
    dialog_sessions: std::collections::HashMap<String, String>,
}

static SESSION_REACHABILITY: OnceLock<
    Mutex<std::collections::HashMap<String, SessionReachabilityProfile>>,
> = OnceLock::new();

fn session_reachability_registry(
) -> &'static Mutex<std::collections::HashMap<String, SessionReachabilityProfile>> {
    SESSION_REACHABILITY.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn lock_session_reachability_registry() -> OmniResult<
    std::sync::MutexGuard<'static, std::collections::HashMap<String, SessionReachabilityProfile>>,
> {
    session_reachability_registry()
        .lock()
        .map_err(|_| OmniError::Internal("session reachability registry lock is poisoned".to_string()))
}

/// Запоминает, какой сессии принадлежит диалог. Зовётся оттуда, где обе величины уже известны.
fn link_session_to_dialog(db_path: &str, dialog_key: &str, session_key: &str) {
    let dialog_key = normalize_dialog_key(dialog_key);
    if dialog_key.trim().is_empty() || session_key.trim().is_empty() {
        return;
    }
    if let Ok(mut registry) = lock_session_reachability_registry() {
        registry
            .entry(db_path.to_string())
            .or_default()
            .dialog_sessions
            .insert(dialog_key, session_key.to_string());
    }
}

fn session_key_for_dialog(db_path: &str, dialog_key: &str) -> Option<String> {
    let dialog_key = normalize_dialog_key(dialog_key);
    let registry = lock_session_reachability_registry().ok()?;
    registry.get(db_path)?.dialog_sessions.get(&dialog_key).cloned()
}

fn drop_expired_session_probes(entry: &mut SessionReachabilityEntry, now_unix_ms: i64) {
    while entry
        .unanswered
        .front()
        .is_some_and(|probe| now_unix_ms.saturating_sub(probe.sent_at_unix_ms) > SESSION_PROBE_EXPIRY_MS)
    {
        entry.unanswered.pop_front();
    }
}

/// Мы отправили пиру пробу. Сам факт отправки ничего о достижимости не говорит — он лишь заводит
/// ожидание ответа, по возрасту которого потом видно, как долго мы в неизвестности.
fn note_session_probe_sent(
    db_path: &str,
    session_key: &str,
    ping_id: &str,
    sent_at_unix_ms: i64,
) -> OmniResult<()> {
    if session_key.trim().is_empty() || ping_id.trim().is_empty() {
        return Ok(());
    }
    let unanswered = {
        let mut registry = lock_session_reachability_registry()?;
        let entry = registry
            .entry(db_path.to_string())
            .or_default()
            .sessions
            .entry(session_key.to_string())
            .or_default();
        drop_expired_session_probes(entry, sent_at_unix_ms);
        entry.unanswered.push_back(SessionProbe {
            ping_id: ping_id.to_string(),
            sent_at_unix_ms,
        });
        while entry.unanswered.len() > SESSION_PROBE_TRACKING_LIMIT {
            entry.unanswered.pop_front();
        }
        entry.probes_sent = entry.probes_sent.saturating_add(1);
        entry.unanswered.len()
    };
    omni_app_trace_log(
        "session/probe",
        &format!("session={session_key} ping_id={ping_id} unanswered={unanswered}"),
    );
    Ok(())
}

/// Пир ответил. Это единственное во всём стеке доказательство, что НАШ исходящий плечо живо:
/// pong мог появиться только если наш ping доехал. Возвращает `true`, если ответ удалось связать
/// с конкретной пробой (тогда посчитан и RTT).
fn note_session_probe_answered(
    db_path: &str,
    session_key: &str,
    ping_id: &str,
    observed_at_unix_ms: i64,
) -> OmniResult<bool> {
    if session_key.trim().is_empty() {
        return Ok(false);
    }
    let (correlated, round_trip_ms, unanswered, oldest_unanswered_at) = {
        let mut registry = lock_session_reachability_registry()?;
        let entry = registry
            .entry(db_path.to_string())
            .or_default()
            .sessions
            .entry(session_key.to_string())
            .or_default();
        drop_expired_session_probes(entry, observed_at_unix_ms);

        let matched = entry
            .unanswered
            .iter()
            .position(|probe| probe.ping_id == ping_id);
        let round_trip_ms = match matched {
            Some(index) => {
                let sent_at_unix_ms = entry.unanswered[index].sent_at_unix_ms;
                // Всё, что отправлено не позже отвеченной пробы, доказанным кругом перекрыто.
                entry.unanswered.drain(..=index);
                Some(observed_at_unix_ms.saturating_sub(sent_at_unix_ms).max(0))
            }
            None => {
                entry.uncorrelated_answers = entry.uncorrelated_answers.saturating_add(1);
                None
            }
        };
        // Ответ пришёл — значит круг состоялся, независимо от того, нашлась ли своя проба.
        entry.acknowledged_at_unix_ms = Some(
            entry
                .acknowledged_at_unix_ms
                .unwrap_or(observed_at_unix_ms)
                .max(observed_at_unix_ms),
        );
        if round_trip_ms.is_some() {
            entry.round_trip_ms = round_trip_ms;
        }
        entry.probes_answered = entry.probes_answered.saturating_add(1);
        (
            round_trip_ms.is_some(),
            round_trip_ms,
            entry.unanswered.len(),
            entry.unanswered.front().map(|probe| probe.sent_at_unix_ms),
        )
    };
    let age_ms = oldest_unanswered_at
        .map(|sent_at| observed_at_unix_ms.saturating_sub(sent_at))
        .unwrap_or_default();
    omni_app_trace_log(
        "session/ack",
        &format!(
            "session={session_key} ping_id={ping_id} correlated={correlated} rtt_ms={} unanswered={unanswered} oldest_unanswered_age_ms={age_ms}",
            round_trip_ms
                .map(|value| value.to_string())
                .unwrap_or_else(|| "none".to_string())
        ),
    );
    Ok(correlated)
}

/// Что сессия знает о своей достижимости прямо сейчас. `None` — про эту сессию мы не знаем ничего
/// (ни одной пробы с момента старта), и это честный ответ, а не «всё хорошо».
#[allow(dead_code)] // Потребители появляются на шаге 2 (вердикт) и шаге 3 (перевод гейтов).
fn session_acknowledgement(db_path: &str, session_key: &str) -> Option<SessionAcknowledgement> {
    let registry = lock_session_reachability_registry().ok()?;
    let entry = registry.get(db_path)?.sessions.get(session_key)?;
    Some(SessionAcknowledgement {
        acknowledged_at_unix_ms: entry.acknowledged_at_unix_ms,
        round_trip_ms: entry.round_trip_ms,
        unanswered_probes: entry.unanswered.len(),
        oldest_unanswered_probe_at_unix_ms: entry
            .unanswered
            .front()
            .map(|probe| probe.sent_at_unix_ms),
        probes_sent: entry.probes_sent,
        probes_answered: entry.probes_answered,
        uncorrelated_answers: entry.uncorrelated_answers,
    })
}

/// На этот диалог поднялся НОВЫЙ линк: пробы, оставшиеся без ответа на прежнем (или пока линка не
/// было вовсе), про новый ничего не доказывают. Без сброса восстановление «мёртвого исходящего
/// плеча» срабатывало через доли секунды после подъёма свежего здорового линка по улике, накопленной
/// за время обрыва, и уводило его на TCP-дверь (живьём 2026-10-06: `dial/race won` по UDP, через
/// 280 мс `drop_phantom ... routed_over_door` по `unanswered@79431ms`). Подтверждения и счётчики
/// не трогаем: это история, а не улика.
fn restart_session_probe_window_for_dialog(db_path: &str, dialog_key: &str) {
    let Some(session_key) = session_key_for_dialog(db_path, dialog_key) else {
        return;
    };
    if let Ok(mut registry) = lock_session_reachability_registry() {
        if let Some(entry) = registry
            .get_mut(db_path)
            .and_then(|profile| profile.sessions.get_mut(&session_key))
        {
            entry.unanswered.clear();
        }
    }
}

/// Наш исходящий поток к пиру этого диалога продвинулся (см. `outbound_progress_at_unix_ms`).
fn note_session_outbound_progress_for_dialog(db_path: &str, dialog_key: &str, now_unix_ms: i64) {
    let Some(session_key) = session_key_for_dialog(db_path, dialog_key) else {
        return;
    };
    if let Ok(mut registry) = lock_session_reachability_registry() {
        if let Some(entry) = registry
            .get_mut(db_path)
            .and_then(|profile| profile.sessions.get_mut(&session_key))
        {
            entry.outbound_progress_at_unix_ms = Some(now_unix_ms);
        }
    }
}

/// Пир доказал, что слышит нас, не ответом на пробу, а тем, что наш поток к нему продвигается.
fn outbound_progress_is_recent(entry: &SessionReachabilityEntry, now_unix_ms: i64) -> bool {
    entry
        .outbound_progress_at_unix_ms
        .is_some_and(|at| now_unix_ms.saturating_sub(at) < SESSION_ACK_DEAD_MS)
}

/// Контакт забыт — вместе с его сессиями исчезает и всё, что мы знали об их достижимости.
/// Ключ сессии — `remote_account_id::device_id`, поэтому у одного аккаунта их может быть несколько
/// (мультиустройство); чистим все, как это делает `delete_session_state_for_remote_account`.
fn forget_session_reachability_for_remote_account(db_path: &str, remote_account_id: &str) {
    if remote_account_id.trim().is_empty() {
        return;
    }
    let prefix = format!("{remote_account_id}::");
    if let Ok(mut registry) = lock_session_reachability_registry() {
        if let Some(profile) = registry.get_mut(db_path) {
            profile
                .sessions
                .retain(|session_key, _| !session_key.starts_with(&prefix));
            profile
                .dialog_sessions
                .retain(|_, session_key| !session_key.starts_with(&prefix));
        }
    }
}

fn clear_session_reachability_for_profile(db_path: &str) {
    if let Ok(mut registry) = lock_session_reachability_registry() {
        registry.remove(db_path);
    }
}

// --- ШАГ 2: троичный вердикт ------------------------------------------------------------------
//
// Двоичного не хватает, потому что есть durable store-and-forward: «не доставлю сейчас» и «не
// доставлю вообще» — разные ответы, и сегодня они слиты в `outcome=sent`, отчего галочка
// «доставлено» появляется на сообщении, которое лежит на хранении.
//
// Функция пока НИЧЕГО не решает: её результат печатается рядом с решением несущей, чтобы на живом
// канале увидеть, где они расходятся, прежде чем передавать право приговора. Шаг 3 — перевод
// потребителей по одному.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionDeliveryVerdict {
    /// Доставлю сейчас.
    DeliverNow,
    /// Прямо сейчас не доставлю, но durable примет на хранение. Не ошибка — штатный режим.
    DeliverLater,
    /// Ни пути, ни durable.
    CannotDeliver,
}

impl SessionDeliveryVerdict {
    fn as_trace_value(self) -> &'static str {
        match self {
            Self::DeliverNow => "now",
            Self::DeliverLater => "later",
            Self::CannotDeliver => "cannot",
        }
    }
}

/// На чём вердикт основан. Отдельно от вердикта намеренно: «не знаю» обязано быть видно, иначе оно
/// снова закодируется как «всё хорошо» (или, что не лучше, как «всё плохо»).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionDeliveryEvidence {
    /// Пир ответил на нашу пробу столько-то мс назад — прямое доказательство, что нас слышат.
    PeerAcknowledged { age_ms: i64 },
    /// Пробы уходят, ответов нет уже столько-то мс — прямое доказательство обратного.
    ProbesUnanswered { age_ms: i64 },
    /// Про эту сессию мы не знаем ничего: не пробовали, только что стартовали, диалог не открыт.
    NotProbed,
}

impl SessionDeliveryEvidence {
    pub(crate) fn as_trace_value(self) -> String {
        match self {
            Self::PeerAcknowledged { age_ms } => format!("acknowledged@{age_ms}ms"),
            Self::ProbesUnanswered { age_ms } => format!("unanswered@{age_ms}ms"),
            Self::NotProbed => "not_probed".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionDeliveryAssessment {
    pub(crate) verdict: SessionDeliveryVerdict,
    pub(crate) evidence: SessionDeliveryEvidence,
}

/// Свежее этого подтверждение считается действующим. Три такта пробы (`OPEN_CHAT_KEEPALIVE_INTERVAL_MS`)
/// — одна-две потери подряд не должны ронять вердикт.
const SESSION_ACK_FRESH_MS: i64 = 3 * OPEN_CHAT_KEEPALIVE_INTERVAL_MS;

/// Столько без единого ответа при уходящих пробах — исходящее плечо считается мёртвым, что бы ни
/// говорила несущая. Совпадает с `CHAT_SESSION_KEEPALIVE_STALE_AFTER_MS` не случайно: это тот же
/// порог «давно не слышали», только выведенный из доказательства, а не из факта открытого диалога.
const SESSION_ACK_DEAD_MS: i64 = 4 * OPEN_CHAT_KEEPALIVE_INTERVAL_MS;

fn session_delivery_evidence(
    acknowledgement: Option<&SessionAcknowledgement>,
    now_unix_ms: i64,
) -> SessionDeliveryEvidence {
    let Some(acknowledgement) = acknowledgement else {
        return SessionDeliveryEvidence::NotProbed;
    };
    let acknowledged_age_ms = acknowledgement
        .acknowledged_at_unix_ms
        .map(|at| now_unix_ms.saturating_sub(at).max(0));
    if let Some(age_ms) = acknowledged_age_ms {
        if age_ms < SESSION_ACK_FRESH_MS {
            return SessionDeliveryEvidence::PeerAcknowledged { age_ms };
        }
    }
    let unanswered_age_ms = acknowledgement
        .oldest_unanswered_probe_at_unix_ms
        .map(|at| now_unix_ms.saturating_sub(at).max(0));
    if let Some(age_ms) = unanswered_age_ms {
        if age_ms >= SESSION_ACK_DEAD_MS {
            return SessionDeliveryEvidence::ProbesUnanswered { age_ms };
        }
    }
    match acknowledged_age_ms {
        // Устаревшее подтверждение — всё ещё доказательство, просто не действующее.
        Some(age_ms) => SessionDeliveryEvidence::PeerAcknowledged { age_ms },
        None if acknowledgement.probes_sent > 0 => {
            SessionDeliveryEvidence::ProbesUnanswered {
                age_ms: unanswered_age_ms.unwrap_or_default(),
            }
        }
        None => SessionDeliveryEvidence::NotProbed,
    }
}

/// Вердикт по сессии. `carrier_has_path` и `durable_available` — это СПОСОБНОСТЬ несущей, вход для
/// выбора, куда положить пакет; ответа «дойдёт ли» они не дают.
fn assess_session_delivery(
    acknowledgement: Option<&SessionAcknowledgement>,
    carrier_has_path: bool,
    durable_available: bool,
    now_unix_ms: i64,
) -> SessionDeliveryAssessment {
    let evidence = session_delivery_evidence(acknowledgement, now_unix_ms);
    let fallback = if durable_available {
        SessionDeliveryVerdict::DeliverLater
    } else {
        SessionDeliveryVerdict::CannotDeliver
    };
    let verdict = match evidence {
        // Доказательство сильнее способности: пир только что нам ответил, и неважно, что несущая
        // думает про свои пути.
        SessionDeliveryEvidence::PeerAcknowledged { age_ms } if age_ms < SESSION_ACK_FRESH_MS => {
            SessionDeliveryVerdict::DeliverNow
        }
        // Плечо доказано мёртвым — `has_link` тут не аргумент. Ровно этот случай дал 496
        // `outcome=sent` при нулевом входящем.
        SessionDeliveryEvidence::ProbesUnanswered { age_ms } if age_ms >= SESSION_ACK_DEAD_MS => {
            fallback
        }
        // Доказательств нет или они в серой зоне — отвечает способность несущей. «Не знаю» НЕ
        // кодируется ни как «всё хорошо», ни как «не доставлю».
        _ => {
            if carrier_has_path {
                SessionDeliveryVerdict::DeliverNow
            } else {
                fallback
            }
        }
    };
    SessionDeliveryAssessment { verdict, evidence }
}

/// Сколько прошло с тех пор, как пир в последний раз доказал, что слышит нас. `None` — не доказывал
/// ни разу (или про эту сессию мы вообще ничего не знаем), и это НЕ то же самое, что «давно»:
/// у вызывающего должно быть право отличить незнание от старости.
///
/// Порог сравнения намеренно не зашит: расписание проб и срок годности вердикта — разные вопросы,
/// и связывать их одной константой значит потом менять одно и молча ломать другое.
fn session_acknowledgement_age_for_dialog(
    db_path: &str,
    dialog_key: &str,
    now_unix_ms: i64,
) -> Option<i64> {
    let session_key = session_key_for_dialog(db_path, dialog_key)?;
    session_acknowledgement(db_path, &session_key)
        .and_then(|acknowledgement| acknowledgement.acknowledged_at_unix_ms)
        .map(|at| now_unix_ms.saturating_sub(at).max(0))
}

/// То же, но по аккаунту контакта, а не по диалогу: интерфейсу диалог не нужен, ему нужен ответ
/// про контакта. Ключ сессии — `account::device`, поэтому берём самое свежее подтверждение среди
/// устройств аккаунта: достижим контакт, а не конкретная его железка.
fn session_acknowledgement_age_for_remote_account(
    db_path: &str,
    remote_account_id: &str,
    now_unix_ms: i64,
) -> Option<i64> {
    if remote_account_id.trim().is_empty() {
        return None;
    }
    let prefix = format!("{remote_account_id}::");
    let registry = lock_session_reachability_registry().ok()?;
    registry
        .get(db_path)?
        .sessions
        .iter()
        .filter(|(session_key, _)| session_key.starts_with(&prefix))
        .filter_map(|(_, entry)| entry.acknowledged_at_unix_ms)
        .max()
        .map(|at| now_unix_ms.saturating_sub(at).max(0))
}

/// Доказано ли, что мы не достучимся до контакта: НИ ОДНО из его устройств не отвечает на пробы
/// дольше порога. Если хоть одно отвечает — контакт достижим.
fn session_outbound_leg_is_proven_dead_for_remote_account(
    db_path: &str,
    remote_account_id: &str,
    now_unix_ms: i64,
) -> bool {
    if remote_account_id.trim().is_empty() {
        return false;
    }
    let prefix = format!("{remote_account_id}::");
    let Ok(registry) = lock_session_reachability_registry() else {
        return false;
    };
    let Some(profile) = registry.get(db_path) else {
        return false;
    };
    let mut saw_session = false;
    for (session_key, entry) in profile.sessions.iter() {
        if !session_key.starts_with(&prefix) {
            continue;
        }
        saw_session = true;
        if outbound_progress_is_recent(entry, now_unix_ms) {
            return false;
        }
        let acknowledgement = SessionAcknowledgement {
            acknowledged_at_unix_ms: entry.acknowledged_at_unix_ms,
            round_trip_ms: entry.round_trip_ms,
            unanswered_probes: entry.unanswered.len(),
            oldest_unanswered_probe_at_unix_ms: entry
                .unanswered
                .front()
                .map(|probe| probe.sent_at_unix_ms),
            probes_sent: entry.probes_sent,
            probes_answered: entry.probes_answered,
            uncorrelated_answers: entry.uncorrelated_answers,
        };
        let dead = matches!(
            session_delivery_evidence(Some(&acknowledgement), now_unix_ms),
            SessionDeliveryEvidence::ProbesUnanswered { age_ms } if age_ms >= SESSION_ACK_DEAD_MS
        );
        if !dead {
            return false;
        }
    }
    saw_session
}

/// Доказано ли, что наше исходящее плечо мертво. Это единственное основание чинить путь: «на канале
/// тихо» им не является — тишина бывает и на исправной связи.
fn session_outbound_leg_is_proven_dead(db_path: &str, dialog_key: &str, now_unix_ms: i64) -> bool {
    let Some(session_key) = session_key_for_dialog(db_path, dialog_key) else {
        return false;
    };
    let progressing = lock_session_reachability_registry().ok().is_some_and(|registry| {
        registry
            .get(db_path)
            .and_then(|profile| profile.sessions.get(&session_key))
            .is_some_and(|entry| outbound_progress_is_recent(entry, now_unix_ms))
    });
    if progressing {
        return false;
    }
    let Some(acknowledgement) = session_acknowledgement(db_path, &session_key) else {
        return false;
    };
    matches!(
        session_delivery_evidence(Some(&acknowledgement), now_unix_ms),
        SessionDeliveryEvidence::ProbesUnanswered { age_ms } if age_ms >= SESSION_ACK_DEAD_MS
    )
}

/// Разрешено ли сейчас пытаться чинить путь к этому пиру. Разрешение выдаётся не чаще, чем раз в
/// `SESSION_ACK_DEAD_MS`: между попытками должно уместиться целое окно проб, иначе мы будем сносить
/// путь быстрее, чем успеваем понять, ожил ли он.
fn allow_session_recovery_attempt(db_path: &str, dialog_key: &str, now_unix_ms: i64) -> bool {
    let Some(session_key) = session_key_for_dialog(db_path, dialog_key) else {
        return false;
    };
    let Ok(mut registry) = lock_session_reachability_registry() else {
        return false;
    };
    let Some(entry) = registry
        .get_mut(db_path)
        .and_then(|profile| profile.sessions.get_mut(&session_key))
    else {
        return false;
    };
    let too_soon = entry
        .recovery_attempted_at_unix_ms
        .is_some_and(|at| now_unix_ms.saturating_sub(at) < SESSION_ACK_DEAD_MS);
    if too_soon {
        return false;
    }
    entry.recovery_attempted_at_unix_ms = Some(now_unix_ms);
    true
}

/// Вердикт по диалогу: находит сессию, которой диалог принадлежит, и печатает `session/verdict`
/// рядом с решением несущей. Печать оставлена и после шага 3 — по полю `agrees` видно, где новый
/// владелец приговора расходится со старым.
fn session_delivery_for_dialog(
    db_path: &str,
    dialog_key: &str,
    carrier_has_path: bool,
    durable_available: bool,
) -> SessionDeliveryAssessment {
    let session_key = session_key_for_dialog(db_path, dialog_key);
    let acknowledgement = session_key
        .as_deref()
        .and_then(|session_key| session_acknowledgement(db_path, session_key));
    let assessment = assess_session_delivery(
        acknowledgement.as_ref(),
        carrier_has_path,
        durable_available,
        current_unix_ms(),
    );
    let carrier_says_now = carrier_has_path;
    let session_says_now = matches!(assessment.verdict, SessionDeliveryVerdict::DeliverNow);
    omni_app_trace_log(
        "session/verdict",
        &format!(
            "dialog_key={dialog_key} session={} verdict={} evidence={} carrier_path={carrier_has_path} agrees={}",
            session_key.as_deref().unwrap_or("unknown"),
            assessment.verdict.as_trace_value(),
            assessment.evidence.as_trace_value(),
            carrier_says_now == session_says_now
        ),
    );
    assessment
}
