// Повторная отправка не должна жечь счётчик сессии.
//
// docs/bugs/session-wedged-by-counter-gap.md — часть 1.
//
// Каждая попытка отправки строила НОВЫЙ шифрованный конверт с новым `send_counter`. При такте
// ретраев ~4 в секунду окно приёма (`MAX_RECEIVE_GAP` = 64) вылетало за ~16 секунд, после чего
// приёмник отвергал ВСЁ, включая presence-пинги, — а раз ответа нет, сообщение не подтверждалось и
// очередь ретраила дальше. Петля сама себя кормила: на живом замере 955 отказов
// «counter exceeded the supported receive gap» при идеальном транспорте (LAN, RTT 10 мс, потерь 0).
//
// Здесь конверт запоминается на время жизни сообщения в очереди и переотправляется как есть. Тогда
// разрыв счётчика ограничен числом РАЗНЫХ сообщений, а не числом попыток, и частые ретраи
// перестают быть смертельными. Повтор того же конверта приёмник уже умеет: он опознаёт его как
// повтор по счётчику (в том же логе 50 таких — штатный случай, конверт приехал двумя путями).
//
// Кэш РАНТАЙМНЫЙ: ретраи происходят, пока приложение работает, а после перезапуска одна пересборка
// на сообщение безвредна и ограничена. Персистить ради этого нечего — цена ошибки тут не потеря
// данных, а лишний счётчик.

/// Ключ КОНКРЕТНОЙ сессии, а не пира.
///
/// `SessionAddress::storage_key` — это пара (account_id, device_id). Она переживает пересборку
/// сессии: аккаунт и устройство у пира те же, меняются только ключи. Пока кэш конвертов
/// инвалидировался по ней, после пересборки из него доставался конверт, зашифрованный МЁРТВОЙ
/// сессией, — приёмник отвечал на него `authentication failed`, и так до бесконечности, потому что
/// повтор доставал из кэша тот же самый конверт. На живом двухмашинном прогоне это дало 51 отказ
/// подряд и очередь, которая не разбиралась вовсе.
/// См. docs/bugs/queue-never-drains-after-relink.md.
///
/// Ратчет-ключ рождается заново на каждом X3DH, поэтому он и отличает экземпляры сессии.
fn session_instance_key(session: &omni_crypto::SessionRecord) -> String {
    format!(
        "{}#{}",
        session.session_address.storage_key(),
        omni_crypto::ephemeral_public_key_to_wire(session.local_ratchet_public_key),
    )
}

/// Запомненный конверт: сам конверт плюс сессия, для которой он собран.
#[derive(Clone)]
struct CachedOutgoingEnvelope {
    /// Ключ экземпляра сессии (`session_instance_key`) на момент сборки. Пересобранная сессия
    /// делает конверт бесполезным — расшифровать его новым состоянием нельзя, поэтому кэш по этому
    /// полю и инвалидируется.
    session_key: String,
    envelope: omni_types::WireEnvelopeV1,
}

/// Потолок на профиль. Ограничивает память при большой залежавшейся очереди; вытесняется самая
/// старая запись, и её сообщение просто пересоберётся на следующей попытке.
const OUTGOING_ENVELOPE_CACHE_CAP: usize = 256;

static OUTGOING_ENVELOPES: OnceLock<
    Mutex<
        std::collections::HashMap<
            String,
            std::collections::VecDeque<(String, CachedOutgoingEnvelope)>,
        >,
    >,
> = OnceLock::new();

fn outgoing_envelope_registry() -> &'static Mutex<
    std::collections::HashMap<String, std::collections::VecDeque<(String, CachedOutgoingEnvelope)>>,
> {
    OUTGOING_ENVELOPES.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Взять запомненный конверт, если он собран для ТОЙ ЖЕ сессии. Иначе `None` — вызывающий соберёт
/// новый (и потратит счётчик, что здесь уже правильно: сессия другая).
fn cached_outgoing_envelope(
    db_path: &str,
    message_id: &str,
    session_key: &str,
) -> Option<omni_types::WireEnvelopeV1> {
    let registry = outgoing_envelope_registry().lock().ok()?;
    let entries = registry.get(db_path)?;
    let (_, cached) = entries.iter().find(|(id, _)| id == message_id)?;
    if cached.session_key != session_key {
        return None;
    }
    Some(cached.envelope.clone())
}

/// Сколько РАЗНЫХ неподтверждённых конвертов уже выложено на эту сессию.
///
/// Это и есть разрыв, который увидит приёмник: каждый такой конверт — потраченный счётчик, а запись
/// снимается по расписке о доставке. Число нужно отправителю, чтобы не переступать окно приёма.
fn outgoing_envelopes_in_flight_for_session(db_path: &str, session_key: &str) -> usize {
    let Ok(registry) = outgoing_envelope_registry().lock() else {
        return 0;
    };
    let Some(entries) = registry.get(db_path) else {
        return 0;
    };
    entries
        .iter()
        .filter(|(_, cached)| cached.session_key == session_key)
        .count()
}

fn remember_outgoing_envelope(
    db_path: &str,
    message_id: &str,
    session_key: &str,
    envelope: &omni_types::WireEnvelopeV1,
) {
    let Ok(mut registry) = outgoing_envelope_registry().lock() else {
        return;
    };
    let entries = registry.entry(db_path.to_string()).or_default();
    entries.retain(|(id, _)| id != message_id);
    entries.push_back((
        message_id.to_string(),
        CachedOutgoingEnvelope {
            session_key: session_key.to_string(),
            envelope: envelope.clone(),
        },
    ));
    while entries.len() > OUTGOING_ENVELOPE_CACHE_CAP {
        entries.pop_front();
    }
}

/// Сообщение больше не в полёте (подтверждено или похоронено) — держать его конверт незачем.
fn forget_outgoing_envelope(db_path: &str, message_id: &str) {
    if let Ok(mut registry) = outgoing_envelope_registry().lock() {
        if let Some(entries) = registry.get_mut(db_path) {
            entries.retain(|(id, _)| id != message_id);
        }
    }
}

fn clear_outgoing_envelopes_for_profile(db_path: &str) {
    if let Ok(mut registry) = outgoing_envelope_registry().lock() {
        registry.remove(db_path);
    }
}
