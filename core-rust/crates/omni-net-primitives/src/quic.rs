// Identity-agnostic QUIC primitives. Extracted из
// omni-transport/quic_carrier.rs (2026-05-28, Strategy B).
//
// Что здесь:
// - `OmniServerCertVerifier`: TLS verifier который accept'ит любой
//   server cert (no PKI) но делает **настоящую** signature verification.
//   App-level identity verification (instance_id hello / NodeAddress
//   challenge) выполняется выше, поверх QUIC.
// - `bind_quic_udp_socket`: socket2 UDP bind (без SO_REUSEADDR) + nonblocking.
// - `omni_transport_config` / `omni_server_config` / `omni_client_config`:
//   QUIC config builders, parameterized by ALPN (the caller passes it; the live mesh uses `h3`
//   to masquerade as HTTP/3 — see omni-network `OMNI_MESH_ALPN`).
//
// Чего здесь НЕТ: handshake protocols, instance_id, NodeAddress. Это
// app-specific layer'ы поверх.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use quinn::rustls::crypto::CryptoProvider;
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use quinn::rustls::{ClientConfig as RustlsClientConfig, DigitallySignedStruct, SignatureScheme};
use quinn::{ClientConfig, ServerConfig, TransportConfig};
use thiserror::Error;

/// Defaults — изначально те же что были в omni-transport (numeric tunables 1:1 при extraction'е).
/// `QUIC_MAX_IDLE_TIMEOUT` с тех пор снижен 60→30 с (см. ниже).
pub const QUIC_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(10);
/// Idle timeout before quinn declares a connection dead (sets `close_reason()`). Lowered 60→30 с
/// to halve recovery: `QuicInterface::dial` reuses a cached connection while `close_reason().is_none()`,
/// so after a relay/peer restart a re-dial keeps returning the DEAD-but-not-yet-timed-out connection
/// until this fires — that was the ~60 с floor on TCP-fallback self-heal. 30 с stays a safe 3× the 10 с
/// keep-alive (a live link round-trips a keep-alive every 10 с, so it never idles; only a genuinely
/// dead one reaches 30 с). Effective per-connection idle is `min()` of both peers, so this speeds up
/// recovery even against an older relay still advertising 60 с. See docs/decisions/tcp-relay-fallback.md.
pub const QUIC_MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
pub const QUIC_DATAGRAM_RECEIVE_BUFFER_BYTES: usize = 2 * 1024 * 1024;
/// Flow-control windows so a bulk transfer keeps the pipe full while the receiver drains a chunk to
/// disk. quinn's default per-stream window (~1.25 MB) is barely ONE attachment chunk, so the sender
/// cannot run ahead and the transfer becomes a ping-pong: per-stream totals showed the sender blocked
/// 903 ms of a 1682 ms stream while the receiver sat 807 ms starved. Widening cut both.
pub const QUIC_STREAM_RECEIVE_WINDOW_BYTES: u64 = 8 * 1024 * 1024;
pub const QUIC_CONNECTION_WINDOW_BYTES: u64 = 32 * 1024 * 1024;
/// Ceiling for path-MTU discovery, and the largest datagram an endpoint accepts. quinn defaults both
/// to 1452, which is not a property of the path -- it is just where it stops probing. Segmentation
/// offload already batches (512 transmit / 64 receive segments on this socket), so syscall count is
/// no longer the cost; the kernel still walks every datagram inside a batch, and throughput tracks
/// PACKET COUNT. A jumbo-capable path carries 9000: six times fewer packets for the same bytes.
///
/// This forces nothing on anyone. quinn runs DPLPMTUD, so each path probes upward and settles on what
/// gets through, with QUIC 1200-byte floor and black-hole detection behind it. Not a risk for the
/// restricted-mobile case either: that path drops QUIC by HEADER SIGNATURE, not by size -- raw UDP of
/// any size measured 8/8 through . quic/stats reports the
/// negotiated mtu and blackholes per connection, so a misbehaving path stays visible.
pub const QUIC_MAX_UDP_PAYLOAD_BYTES: u16 = 9000;
/// Kernel UDP socket buffer, per direction. Windows defaults to roughly 64 KB, under a millisecond at
/// gigabit -- any scheduling hiccup becomes packet loss that QUIC pays for as congestion. It got sharp
/// once MTU discovery was allowed past 1452: at 9000-byte datagrams that 64 KB holds ~7 packets, and a
/// transfer showed 4.21% loss with a collapsed window. At 4 MB (~30 ms of gigabit) the same run
/// measures 0.00%.
pub const QUIC_SOCKET_BUFFER_BYTES: usize = 4 * 1024 * 1024;
pub const QUIC_MAX_BIDI_STREAMS: u32 = 64;
pub const QUIC_MAX_UNI_STREAMS: u32 = 256;

#[derive(Debug, Error)]
pub enum QuicPrimitiveError {
    #[error("UDP socket setup failed: {0}")]
    Socket(String),
    #[error("TLS/QUIC config build failed: {0}")]
    Config(String),
}

/// Bind a non-blocking UDP socket suitable для a quinn Endpoint. IPv6 unspecified binds dual-stack.
///
/// No `SO_REUSEADDR`: a unicast QUIC socket has nothing to gain from it (UDP has no TIME_WAIT), and
/// on Windows it let a second socket bind a port that is IN USE and split its datagrams with the
/// first. That was harmless while every start took a random port; mesh now asks for the port of its
/// previous run, and a held port must fail the bind so it falls back to a fresh one.
pub fn bind_quic_udp_socket(
    bind_addr: SocketAddr,
) -> Result<std::net::UdpSocket, QuicPrimitiveError> {
    let domain = if bind_addr.is_ipv6() {
        socket2::Domain::IPV6
    } else {
        socket2::Domain::IPV4
    };
    let socket = socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))
        .map_err(|e| QuicPrimitiveError::Socket(format!("create: {e}")))?;
    if bind_addr.ip().is_unspecified() && bind_addr.is_ipv6() {
        let _ = socket.set_only_v6(false);
    }
    socket
        .bind(&bind_addr.into())
        .map_err(|e| QuicPrimitiveError::Socket(format!("bind {bind_addr}: {e}")))?;
    // Best-effort: a platform that refuses the size keeps its default rather than failing the bind.
    let _ = socket.set_recv_buffer_size(QUIC_SOCKET_BUFFER_BYTES);
    let _ = socket.set_send_buffer_size(QUIC_SOCKET_BUFFER_BYTES);
    let socket: std::net::UdpSocket = socket.into();
    socket
        .set_nonblocking(true)
        .map_err(|e| QuicPrimitiveError::Socket(format!("nonblocking: {e}")))?;
    Ok(socket)
}

/// What `OMNI_QUIC_QUEUE_BUDGET_MS` asks of the queue cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueBudget {
    /// Unset (or malformed): the carrier's own budget — `UDP_QUEUE_BUDGET` / `DOOR_QUEUE_BUDGET`.
    Default,
    /// `off` or `0`: no cap at all.
    Off,
    /// A budget set by hand.
    Set(std::time::Duration),
}

/// What `OMNI_QUIC_QUEUE_BUDGET_MS` says: a budget in ms, `off`/`0` for none, anything else (unset,
/// malformed) — the default.
///
/// History of the default over UDP. It was off, and the reason was a reproducibility failure rather
/// than a bad idea. 25 ms was
/// picked and briefly shipped on: the LAN stand showed no throughput cost (769 vs 755 Mbit/s across
/// alternating runs, references flat) and the loopback fast path was untouched (1492/1526 against
/// 1395). But two traced runs at that same 25 ms gave RTT-under-load of 13-34 ms and 33-35 ms,
/// because the cap itself came out 3.9 MB in one and 6.4 MB in the other — the delivery-rate
/// estimate still over-reads by 2-5x, so the budget does not yet mean the number written on it, and
/// a knob whose effect varies twofold between identical runs must not be a default.
///
/// A 10 ms budget does cut LAN latency repeatably (9-14 ms against 33-42) at a cost of about 8%
/// throughput, which is a real dial for anyone who wants it — `OMNI_QUIC_QUEUE_BUDGET_MS=10`.
///
/// Fix the residual over-read first; then the value can be chosen on evidence instead of on a
/// number that drifts. Tables and the whole history: docs/bugs/uplink-goodput-retransmit.md §P1b.
///
/// The over-read was a measuring fault (`bdp_cap`, 2026-10-17: a transfer held by flow control was
/// never sampled); after it the budget was measured again and became the default
/// (`UDP_QUEUE_BUDGET`).
fn queue_budget_from_env(raw: Option<&str>) -> QueueBudget {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return QueueBudget::Default;
    };
    if raw.eq_ignore_ascii_case("off") {
        return QueueBudget::Off;
    }
    match raw.parse::<f64>() {
        Ok(0.0) => QueueBudget::Off,
        Ok(ms) if ms.is_finite() && ms > 0.0 => QueueBudget::Set(std::time::Duration::from_secs_f64(ms / 1000.0)),
        _ => QueueBudget::Default,
    }
}

/// Build shared TransportConfig (stream limits, datagram buffer,
/// keep-alive, idle timeout). Identical для всех Omni QUIC users.
/// Во что обходится пользователю трафик по этому каналу. Ядро САМО этого знать не может: на
/// Windows ответ даёт WinRT (`NetworkInformation.GetConnectionCost`), на Android —
/// `ConnectivityManager`. Поэтому факт подаёт оболочка, как и ключ обёртки секретов.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkCost {
    /// Оболочка ещё не сказала (или платформа не умеет).
    Unknown,
    /// Трафик не считают — домашний интернет, Wi-Fi, Ethernet.
    Unmetered,
    /// Трафик считают: мобильный тариф, роуминг, превышенный лимит.
    Metered,
}

static LINK_COST: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Сообщить ядру стоимость канала. Зовётся оболочкой при старте и при смене сети.
///
/// **Применяется при создании эндпоинта**, то есть на уже поднятом соединении смена не
/// отразится — новое значение возьмёт следующий mesh-start. Врать об этом не будем: живого
/// переключения контроллера у quinn нет.
pub fn set_link_cost(cost: LinkCost) {
    let value = match cost {
        LinkCost::Unknown => 0,
        LinkCost::Unmetered => 1,
        LinkCost::Metered => 2,
    };
    LINK_COST.store(value, std::sync::atomic::Ordering::Relaxed);
}

/// Текущая известная стоимость канала.
pub fn link_cost() -> LinkCost {
    match LINK_COST.load(std::sync::atomic::Ordering::Relaxed) {
        1 => LinkCost::Unmetered,
        2 => LinkCost::Metered,
        _ => LinkCost::Unknown,
    }
}

/// Ручной выбор контроллера из настроек. `Auto` — правило по стоимости канала (по умолчанию).
///
/// Существует затем, что A/B на ОДНОЙ живой сборке — единственный способ честно сравнить
/// контроллеры: железо и сота дрейфуют вдвое за час, и прогоны разных сборок несравнимы.
/// Раньше для этого требовалась переменная окружения, то есть перезапуск процесса.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CongestionPreference {
    Auto,
    Fixed(CongestionChoice),
}

static CONGESTION_PREFERENCE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Разбор значения настройки. Нераспознанное — `None`, а НЕ молчаливое «авто»: перепутанная
/// строка обязана быть видна вызывающему, иначе настройка тихо перестанет работать.
pub fn parse_congestion_preference(raw: &str) -> Option<CongestionPreference> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(CongestionPreference::Auto),
        "bbr" => Some(CongestionPreference::Fixed(CongestionChoice::Bbr)),
        "cubic" => Some(CongestionPreference::Fixed(CongestionChoice::Cubic)),
        "newreno" => Some(CongestionPreference::Fixed(CongestionChoice::NewReno)),
        _ => None,
    }
}

/// Имя настройки для сохранения и показа. Обратно к `parse_congestion_preference`.
pub fn congestion_preference_name(pref: CongestionPreference) -> &'static str {
    match pref {
        CongestionPreference::Auto => "auto",
        CongestionPreference::Fixed(CongestionChoice::Bbr) => "bbr",
        CongestionPreference::Fixed(CongestionChoice::Cubic) => "cubic",
        CongestionPreference::Fixed(CongestionChoice::NewReno) => "newreno",
    }
}

/// Применить выбор. Читается при создании следующего endpoint'а — quinn не умеет менять
/// контроллер на живом соединении (`Connection` даёт только окна), поэтому уже поднятые линки
/// доживают на прежнем до переустановки.
pub fn set_congestion_preference(pref: CongestionPreference) {
    let value = match pref {
        CongestionPreference::Auto => 0u8,
        CongestionPreference::Fixed(CongestionChoice::Bbr) => 1,
        CongestionPreference::Fixed(CongestionChoice::Cubic) => 2,
        CongestionPreference::Fixed(CongestionChoice::NewReno) => 3,
    };
    CONGESTION_PREFERENCE.store(value, std::sync::atomic::Ordering::Relaxed);
}

/// Текущий ручной выбор.
pub fn congestion_preference() -> CongestionPreference {
    match CONGESTION_PREFERENCE.load(std::sync::atomic::Ordering::Relaxed) {
        1 => CongestionPreference::Fixed(CongestionChoice::Bbr),
        2 => CongestionPreference::Fixed(CongestionChoice::Cubic),
        3 => CongestionPreference::Fixed(CongestionChoice::NewReno),
        _ => CongestionPreference::Auto,
    }
}

/// Что применится к СЛЕДУЮЩЕМУ соединению прямо сейчас. Нужно оболочке, чтобы настройка не врала:
/// на «авто» она обязана показать реальный итог правила, а при выставленной `OMNI_QUIC_CC` —
/// показать, что решает переменная, а не переключатель.
pub fn effective_congestion_control() -> CongestionChoice {
    select_congestion_control(
        std::env::var("OMNI_QUIC_CC").ok().as_deref(),
        congestion_preference(),
        link_cost(),
    )
}

/// Какой контроллер перегрузки выбрать.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CongestionChoice {
    Bbr,
    Cubic,
    NewReno,
}

/// ПРАВИЛО: BBR везде, кроме каналов, где трафик считают.
///
/// Так решено по двум замерам, которые противоречат друг другу только на первый взгляд.
///
/// **2026-08-31, обычный интернет (Wi-Fi 2.4 ГГц ноутбука ↔ домашний Ethernet ПК).** Cubic
/// упирается не в канал, а в потери: при 0.6 % и RTT 49 мс его классический предел ≈3.8 Мбит/с —
/// ровно измеренное. Замер передачи файла, чередованием, соседние минуты:
///
/// | путь | Cubic | BBR |
/// |---|---|---|
/// | loopback (широкий, без потерь) | 1502 / 1653 Мбит/с | 1429 / 1603 Мбит/с (ничья) |
/// | ПК → ноут | 27.4 Мбит/с | 46.4–60.9 Мбит/с |
/// | ноут → ПК | 3.6 Мбит/с | **65.7 Мбит/с** |
///
/// При этом аплинк ноутбука независимой пробой — 81.6 Мбит/с: на Cubic мы брали 4 % канала.
/// На живом видеозвонке разница видна прямо: с Cubic регулятор срезал 4000 → 736 кбит/с за
/// 10 секунд и задержка медиа ушла за 2 с, с BBR — ни одного понижения за 47 с при RTT 49 мс.
/// Разбор: docs/bugs/cubic-caps-this-path-at-four-percent.md.
///
/// **2026-08-08, мобильный оператор с полисером.** Там BBR — не выигрыш и был бы вредом:
/// goodput ничья внутри собственного двукратного дрейфа соты, а потери 1.2 % → 32-38 % и
/// проводная цена одного файла 21.5 МБ → 35 МБ. Причина в устройстве: quinn'овский BBR слеп к
/// потерям, и против полисера, который РЕЖЕТ, а не буферизует, его модель не сходится — ровный
/// RTT читается как запас. Разбор: docs/bugs/uplink-goodput-retransmit.md §P1.
///
/// Оба замера верны, и вместе они говорят одно: граница проходит не по «быстро/медленно», а по
/// тому, **считают ли трафик**. Это ровно тот признак, который система сообщает сама.
///
/// `Unknown` разрешается ПО ПЛАТФОРМЕ, и это не половинчатость, а разная априорная вероятность.
/// На настольной машине неопознанный канал почти всегда фиксированный, и уйти в Cubic значило бы
/// подарить те самые 18 раз. На телефоне неопознанный канал сплошь и рядом сотовый — то есть
/// ровно тот случай, который замер 2026-08-08 признал вредным, и ошибка там стоит денег
/// пользователя. Поэтому на Android незнание fail-closed: Cubic, пока оболочка не скажет иначе.
///
/// Это не индульгенция оболочке: сообщать факт она обязана (на Android — через
/// `ConnectivityManager`), иначе безлимитный домашний Wi-Fi телефона поедет на Cubic.
///
/// Приоритет: `OMNI_QUIC_CC` → ручной выбор из настроек → правило по стоимости канала.
/// Переменная окружения стоит ВЫШЕ переключателя нарочно: её ставит скрипт A/B под конкретный
/// прогон, и если бы залежавшаяся настройка её перебивала, замер молча мерил бы не то, что
/// написано в его же логе. Нераспознанное значение переменной — НЕ повод свалиться в умолчание
/// quinn: применяется обычное правило.
pub fn select_congestion_control(
    env: Option<&str>,
    pref: CongestionPreference,
    cost: LinkCost,
) -> CongestionChoice {
    match env.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("bbr") => return CongestionChoice::Bbr,
        Some("cubic") => return CongestionChoice::Cubic,
        Some("newreno") => return CongestionChoice::NewReno,
        _ => {}
    }
    if let CongestionPreference::Fixed(choice) = pref {
        return choice;
    }
    match cost {
        LinkCost::Metered => CongestionChoice::Cubic,
        LinkCost::Unmetered => CongestionChoice::Bbr,
        LinkCost::Unknown => unknown_cost_choice(PLATFORM_DEFAULTS_TO_METERED),
    }
}

/// Полное решение о перегрузке: КАКОЙ контроллер и надо ли держать его под бюджетом очереди.
/// Считается в одном месте нарочно — эти две половины уже разъезжались: обёртка бюджета строила
/// свой Cubic и молча отменяла весь выбор BBR/metered. Здесь это выразимо и проверяется тестом.
pub fn congestion_plan(
    cc_env: Option<&str>,
    pref: CongestionPreference,
    cost: LinkCost,
    queue_budget_env: Option<&str>,
) -> (CongestionChoice, QueueBudget) {
    (
        select_congestion_control(cc_env, pref, cost),
        queue_budget_from_env(queue_budget_env),
    )
}

/// Готовая фабрика quinn под принятое решение. Отдельная функция ровно затем, чтобы «какой
/// контроллер выбран» и «что с ним потом делают» нельзя было развести по разным местам: обёртка
/// бюджета очереди раньше строила свой Cubic и молча отменяла решение.
pub fn controller_factory_for(
    choice: CongestionChoice,
) -> Arc<dyn quinn::congestion::ControllerFactory + Send + Sync> {
    match choice {
        CongestionChoice::Bbr => Arc::new(quinn::congestion::BbrConfig::default()),
        CongestionChoice::NewReno => Arc::new(quinn::congestion::NewRenoConfig::default()),
        CongestionChoice::Cubic => Arc::new(quinn::congestion::CubicConfig::default()),
    }
}

/// Считать ли неопознанный канал платным на ЭТОЙ платформе. Единственная платформенная развилка
/// правила; вынесена сюда, чтобы обе её ветки можно было проверить тестом на любой машине.
const PLATFORM_DEFAULTS_TO_METERED: bool = cfg!(target_os = "android");

/// Во что разрешается «стоимость неизвестна». См. `select_congestion_control`.
pub fn unknown_cost_choice(platform_defaults_to_metered: bool) -> CongestionChoice {
    if platform_defaults_to_metered {
        CongestionChoice::Cubic
    } else {
        CongestionChoice::Bbr
    }
}

/// Сколько байт исходящих ДАТАГРАММ quinn держит в очереди, прежде чем начнёт их выбрасывать.
///
/// Величина, о которой надо знать: при переполнении `Connection::send_datagram` **молча
/// выбрасывает САМЫЕ СТАРЫЕ датаграммы и возвращает `Ok(())`**
/// (quinn-proto `datagrams.rs`: `while outgoing_total > datagram_send_buffer_size { pop_front() }`).
/// То есть приложение считает пакет отправленным, на провод он не выходит, и в `loss_pct` его нет —
/// потерю видит только приёмник.
///
/// Умолчание quinn — 1 МБ. Для видео это **резервуар на секунды**: на 3 Мбит/с это ~2.7 с,
/// то есть и огромная задержка до начала выбрасывания, и выбрасывание кусками ПОСРЕДИ кадра
/// (уходит начало, кадр не собирается, приёмник просит ключевой). Живой замер 2026-08-31 на BBR:
/// приёмник рапортует ~30 % потерь RTP при 2.8 % на проводе и пустой нашей очереди
/// (`queue=0p dropQ=0`) — по исключению остаётся только этот буфер.
///
/// Ручка нужна, чтобы это ПРОВЕРИТЬ A/B на одной сборке, а не чинить вслепую. Умолчание НЕ меняем,
/// пока замер не сказал.
fn datagram_send_buffer_from_env(raw: Option<&str>) -> Option<usize> {
    let raw = raw.map(str::trim).filter(|raw| !raw.is_empty())?;
    let bytes = raw.parse::<usize>().ok()?;
    // Меньше одной датаграммы — это не «маленький буфер», это выключенное медиа. Пол в 16 КБ
    // держит примерно один кадр 720p и не даёт ручке превратиться в способ всё сломать.
    if bytes < 16 * 1024 {
        return None;
    }
    Some(bytes)
}

/// Умолчание quinn для очереди исходящих датаграмм (`TransportConfig::default`, 1 МБ).
const QUINN_DEFAULT_DATAGRAM_SEND_BUFFER_BYTES: usize = 1024 * 1024;

/// Размер очереди исходящих датаграмм: `OMNI_QUIC_DATAGRAM_SEND_BUFFER_BYTES` или умолчание quinn.
/// Единственный источник числа — его читают и конфиг соединения, и прибор заполнения очереди
/// (`dgram_q_peak` в `quic/stats`), которому нужно знать, от чего отсчитывать свободное место.
pub fn datagram_send_buffer_bytes() -> usize {
    datagram_send_buffer_from_env(
        std::env::var("OMNI_QUIC_DATAGRAM_SEND_BUFFER_BYTES")
            .ok()
            .as_deref(),
    )
    .unwrap_or(QUINN_DEFAULT_DATAGRAM_SEND_BUFFER_BYTES)
}

/// What a connection's packets ride under QUIC. Over TCP (a door, a relay's TCP carrier) there is a
/// reliable byte-pipe below that hides loss and queues without bound, so the queue is ours to keep
/// short: such a connection runs with a queue budget by default (`DOOR_QUEUE_BUDGET`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransportCarrier {
    Udp,
    Tcp,
}

/// The queue budget a connection over TCP runs with unless `OMNI_QUIC_QUEUE_BUDGET_MS` says
/// otherwise. Measured live 2026-10-16/17 (laptop Wi-Fi <-> PC, 300 MB, docs/bugs/door-throughput.md):
/// without it BBR kept tens of MB in flight and the door's RTT under load was 1.6-1.9 s — every
/// message and call on the link waited behind the file; with 50 ms it was 42-92 ms, throughput
/// 12% lower in both directions.
pub const DOOR_QUEUE_BUDGET: std::time::Duration = std::time::Duration::from_millis(50);

/// The queue budget of a connection over UDP (the rate cap, `bdp_cap`). Measured live 2026-10-17
/// (laptop Wi-Fi <-> PC, 300 MB, alternating series, docs/bugs/door-throughput.md): BBR without it
/// held RTT under load at 130-160 ms, with 100 ms at 50-75 ms, throughput ~11-12% lower — what a
/// message or a call waits behind a file halves; 50 ms cost 17-30%.
pub const UDP_QUEUE_BUDGET: std::time::Duration = std::time::Duration::from_millis(100);

/// How a connection over `carrier` holds its queue down.
#[derive(Debug, PartialEq, Eq)]
enum QueueCap {
    /// The rate-based cap (`bdp_cap`) — `OMNI_QUIC_QUEUE_BUDGET_MS`, the experiment knob.
    Rate(std::time::Duration),
    /// The delay-based cap (`delay_cap`): over TCP, where the rate we would measure is TCP's.
    Delay(std::time::Duration),
}

/// Over TCP the delay cap, over UDP the rate cap; with the hand-set budget or the carrier's own.
fn queue_cap_for(carrier: TransportCarrier, budget: QueueBudget) -> Option<QueueCap> {
    let set = match budget {
        QueueBudget::Off => return None,
        QueueBudget::Set(set) => Some(set),
        QueueBudget::Default => None,
    };
    Some(match carrier {
        TransportCarrier::Tcp => QueueCap::Delay(set.unwrap_or(DOOR_QUEUE_BUDGET)),
        TransportCarrier::Udp => QueueCap::Rate(set.unwrap_or(UDP_QUEUE_BUDGET)),
    })
}

pub fn omni_transport_config() -> TransportConfig {
    omni_transport_config_for(TransportCarrier::Udp)
}

pub fn omni_transport_config_for(carrier: TransportCarrier) -> TransportConfig {
    let mut transport = TransportConfig::default();
    transport.max_concurrent_bidi_streams(QUIC_MAX_BIDI_STREAMS.into());
    transport.max_concurrent_uni_streams(QUIC_MAX_UNI_STREAMS.into());
    transport.datagram_receive_buffer_size(Some(QUIC_DATAGRAM_RECEIVE_BUFFER_BYTES));
    // См. `datagram_send_buffer_bytes`. Ставится явно, хотя по умолчанию равно quinn'овскому: так
    // размер, от которого прибор считает заполнение очереди, и настоящий размер — одно число.
    transport.datagram_send_buffer_size(datagram_send_buffer_bytes());
    transport.stream_receive_window(
        QUIC_STREAM_RECEIVE_WINDOW_BYTES
            .try_into()
            .expect("valid window"),
    );
    transport.receive_window(
        QUIC_CONNECTION_WINDOW_BYTES
            .try_into()
            .expect("valid window"),
    );
    // Bytes allowed in flight, connection-wide. The default here is deliberately huge because on a
    // fast path it must never be the limit -- but on a POLICED MOBILE UPLINK it is the reason a
    // transfer makes the rest of the phone unusable. Measured on the live stand: sending a file
    // takes the path RTT from 44 ms idle to 400-1284 ms under load, i.e. we fill the carrier's queue
    // to the brim and everything else on that radio -- maps, chat, a call -- queues behind our
    // bytes. Throughput is NOT the problem there (we are at the uplink's ceiling either way); the
    // latency we inflict on the user's other traffic is.
    //
    // A fixed cap cannot be the answer: 32 MB is ~400x the BDP of a 15 Mbit/s / 44 ms mobile uplink,
    // but only ~25x that of a 100 Mbit/s / 100 ms path, and far too small for the LAN case. So this
    // is a knob, to be A/B-measured against `rtt_ms` under load before any default moves. See
    // docs/bugs/uplink-goodput-retransmit.md.
    let send_window = std::env::var("OMNI_QUIC_SEND_WINDOW_BYTES")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|bytes| *bytes >= 64 * 1024)
        .unwrap_or(QUIC_CONNECTION_WINDOW_BYTES);
    transport.send_window(send_window);
    let mut mtu_discovery = quinn::MtuDiscoveryConfig::default();
    mtu_discovery.upper_bound(QUIC_MAX_UDP_PAYLOAD_BYTES);
    transport.mtu_discovery_config(Some(mtu_discovery));
    transport.keep_alive_interval(Some(QUIC_KEEP_ALIVE_INTERVAL));
    transport.max_idle_timeout(Some(
        QUIC_MAX_IDLE_TIMEOUT
            .try_into()
            .expect("valid idle timeout"),
    ));
    // Congestion control — см. `congestion_plan` / `select_congestion_control` ниже, там же замеры.
    let (choice, budget) = congestion_plan(
        std::env::var("OMNI_QUIC_CC").ok().as_deref(),
        congestion_preference(),
        link_cost(),
        std::env::var("OMNI_QUIC_QUEUE_BUDGET_MS").ok().as_deref(),
    );
    let cap = queue_cap_for(carrier, budget);
    let mut controller = controller_factory_for(choice);
    // In-flight cap on top of whichever controller was chosen -- see `UDP_QUEUE_BUDGET` /
    // `DOOR_QUEUE_BUDGET` for the measurements behind the defaults, `queue_budget_from_env` for the
    // history, and bdp_cap for the earlier multiplier rule this replaced (unsound, not merely
    // mis-tuned).
    //
    // `OMNI_QUIC_QUEUE_BUDGET_MS=<ms>` is how many milliseconds of SELF-INFLICTED queue a transfer
    // may add on top of the path's own delay: in-flight is held to `rate * (min_rtt + budget)`,
    // both measured live. A/B it against `rtt_ms` and `lost_pkts` under load on BOTH a bloated path
    // and a fast one; the risk it carries is throttling a path that was never the problem.
    //
    // It wraps the CHOSEN controller. It used to hard-code Cubic here, which silently cancelled the
    // whole BBR/metered decision above whenever the budget was set -- and made the one experiment
    // worth running (BBR held to a queue budget, on the policed mobile path where bare BBR inflates
    // cwnd to 21 MB on a 750 KB BDP) impossible to run at all.
    match cap {
        Some(QueueCap::Rate(budget)) => {
            controller = Arc::new(crate::bdp_cap::BdpCappedControllerFactory::new(controller, budget));
        }
        Some(QueueCap::Delay(budget)) => {
            controller = Arc::new(crate::delay_cap::DelayCappedControllerFactory::new(controller, budget));
        }
        None => {}
    }
    transport.congestion_controller_factory(controller);
    // Loss-detection thresholds. Default: quinn's (3 packets / 9/8 RTT) — behaviour UNCHANGED, and
    // MEASURED 2026-08-09 to be the right default. DO NOT raise these hoping for a lossy-link win.
    //
    // The hypothesis was reasonable: a radio link reorders as a matter of course, the default
    // threshold declares loss after only three later arrivals, so a merely-late packet would cost a
    // retransmit AND half the window. Four back-to-back runs on the live mobile path (threshold 3
    // vs 10, alternating) refuted it, and the counters separated cleanly where goodput did not:
    //
    //   threshold  3 -> loss 0.87 / 2.97 %,  cong  6 /  8,  mtu 1471,  black holes  0 /  0
    //   threshold 10 -> loss 5.46 / 8.57 %,  cong 63 / 67,  mtu 1200,  black holes 10 / 11
    //
    // The damage is not where the knob points. Declaring loss later makes losses arrive in BIGGER
    // BATCHES, and quinn's MTU black-hole detector trips on a run of lost large packets — so it
    // concludes the path cannot carry 1471, drops to the 1200 floor, and then needs 23% more packets
    // for the same bytes, which produces more loss and more batches. A self-reinforcing cascade
    // started by a knob that has nothing to do with MTU. Two runs, ten and eleven detections; the
    // default had zero in both.
    //
    // Also worth remembering: the bulk lane is ONE ordered QUIC stream, so late loss detection
    // directly delays delivery of everything behind the gap. The knob trades away exactly the thing
    // that matters most here.
    //
    // Kept (rather than reverted) so the tested-and-rejected hypothesis stays visible and cheap to
    // re-check on a different path. Details in docs/architecture/test-patterns.md.
    if let Ok(raw) = std::env::var("OMNI_QUIC_LOSS_PACKET_THRESHOLD") {
        if let Ok(threshold) = raw.trim().parse::<u32>() {
            // quinn requires >= 1; below the spec's 3 would declare loss even more eagerly, which is
            // the opposite of the point, so refuse to go under the default rather than silently
            // making things worse.
            if threshold >= 3 {
                transport.packet_threshold(threshold);
            }
        }
    }
    if let Ok(raw) = std::env::var("OMNI_QUIC_LOSS_TIME_THRESHOLD") {
        if let Ok(threshold) = raw.trim().parse::<f32>() {
            if threshold.is_finite() && threshold >= 1.0 {
                transport.time_threshold(threshold);
            }
        }
    }
    transport
}

/// Build a QUIC server config from the given cert + key + ALPN.
/// Caller supplies cert: omni-transport использует rcgen self-signed,
/// omni-network — identity-derived. ALPN распознаёт protocol variant.
pub fn omni_server_config(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    alpn: &[u8],
) -> Result<ServerConfig, QuicPrimitiveError> {
    omni_server_config_for(cert, key, alpn, TransportCarrier::Udp)
}

/// [`omni_server_config`] for connections riding `carrier`.
pub fn omni_server_config_for(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    alpn: &[u8],
    carrier: TransportCarrier,
) -> Result<ServerConfig, QuicPrimitiveError> {
    let mut server_crypto = quinn::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(|e| QuicPrimitiveError::Config(format!("server TLS: {e}")))?;
    server_crypto.alpn_protocols = vec![alpn.to_vec()];
    let mut server_config = ServerConfig::with_crypto(Arc::new(
        QuicServerConfig::try_from(server_crypto)
            .map_err(|e| QuicPrimitiveError::Config(format!("server quic: {e}")))?,
    ));
    server_config.transport_config(Arc::new(omni_transport_config_for(carrier)));
    Ok(server_config)
}

/// Build a QUIC client config с OmniServerCertVerifier + given ALPN.
pub fn omni_client_config(alpn: &[u8]) -> Result<ClientConfig, QuicPrimitiveError> {
    omni_client_config_for(alpn, TransportCarrier::Udp)
}

/// [`omni_client_config`] for connections riding `carrier`.
pub fn omni_client_config_for(alpn: &[u8], carrier: TransportCarrier) -> Result<ClientConfig, QuicPrimitiveError> {
    let mut crypto = RustlsClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(OmniServerCertVerifier::new())
        .with_no_client_auth();
    crypto.alpn_protocols = vec![alpn.to_vec()];
    let mut client_config = ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(crypto)
            .map_err(|e| QuicPrimitiveError::Config(format!("client quic: {e}")))?,
    ));
    client_config.transport_config(Arc::new(omni_transport_config_for(carrier)));
    Ok(client_config)
}

/// TLS server-cert verifier для Omni QUIC connections.
///
/// Accept'ит любой server cert (no CA chain — Omni не использует PKI),
/// но **выполняет настоящую signature verification** (TLS 1.2/1.3 handshake
/// signatures). Это значит TLS обеспечивает confidentiality + integrity
/// канала; identity binding делается выше (instance_id hello или NodeAddress
/// challenge).
#[derive(Debug)]
pub struct OmniServerCertVerifier {
    provider: Arc<CryptoProvider>,
}

impl OmniServerCertVerifier {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            provider: Arc::new(quinn::rustls::crypto::ring::default_provider()),
        })
    }
}

impl ServerCertVerifier for OmniServerCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, quinn::rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, quinn::rustls::Error> {
        quinn::rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, quinn::rustls::Error> {
        quinn::rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn the_queue_budget_is_the_carriers_unless_set_or_turned_off() {
        assert_eq!(queue_budget_from_env(None), QueueBudget::Default);
        assert_eq!(
            queue_budget_from_env(Some(" 10 ")),
            QueueBudget::Set(std::time::Duration::from_millis(10))
        );
        assert_eq!(
            queue_budget_from_env(Some("0.5")),
            QueueBudget::Set(std::time::Duration::from_micros(500)),
            "sub-millisecond budgets are legal; the packet floor is what keeps them survivable"
        );
        assert_eq!(queue_budget_from_env(Some("off")), QueueBudget::Off);
        assert_eq!(queue_budget_from_env(Some("0")), QueueBudget::Off);
        assert_eq!(queue_cap_for(TransportCarrier::Udp, QueueBudget::Default), Some(QueueCap::Rate(UDP_QUEUE_BUDGET)));
        assert_eq!(queue_cap_for(TransportCarrier::Udp, QueueBudget::Off), None);
        assert_eq!(queue_cap_for(TransportCarrier::Tcp, QueueBudget::Off), None);
    }

    #[test]
    fn a_malformed_budget_leaves_the_default() {
        // A typo must neither turn the cap off nor set a budget nobody meant.
        for raw in ["", "  ", "twenty", "-5", "NaN"] {
            assert_eq!(queue_budget_from_env(Some(raw)), QueueBudget::Default, "raw={raw:?}");
        }
    }

    #[test]
    fn bind_udp_socket_on_ephemeral_port() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let socket = bind_quic_udp_socket(addr).expect("bind должен работать");
        let local = socket.local_addr().unwrap();
        assert_eq!(local.ip(), Ipv4Addr::LOCALHOST);
        assert_ne!(local.port(), 0);
    }

    /// ЗАНЯТЫЙ ПОРТ ОБЯЗАН ОТКАЗЫВАТЬ. Mesh пытается занять порт прошлого запуска и при отказе берёт
    /// случайный. С `SO_REUSEADDR` второй сокет на Windows молча садился на тот же порт и делил с
    /// первым датаграммы — вторая копия приложения или второй профиль на той же машине получили бы
    /// чужой QUIC вместо ошибки.
    #[test]
    fn a_port_already_held_is_refused_not_shared() {
        let first = bind_quic_udp_socket(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
            .expect("ephemeral bind");
        let held = first.local_addr().unwrap();
        assert!(
            bind_quic_udp_socket(held).is_err(),
            "второй бинд на {held} обязан упасть, пока первый сокет жив"
        );
        // Положительная половина: отпустили — порт снова свободен (у UDP нет TIME_WAIT).
        drop(first);
        let again = bind_quic_udp_socket(held).expect("освобождённый порт берётся снова");
        assert_eq!(again.local_addr().unwrap(), held);
    }

    /// ПРАВИЛО ВЫБОРА КОНТРОЛЛЕРА: BBR везде, кроме каналов, где трафик считают.
    ///
    /// Оба замера, на которых это стоит, названы в докблоке `select_congestion_control`.
    /// Здесь — что именно правило обещает.
    #[test]
    fn a_manual_choice_overrides_the_rule_and_auto_leaves_it_alone() {
        use CongestionChoice::{Bbr, Cubic};
        use CongestionPreference::{Auto, Fixed};

        // Ручной выбор бьёт правило в ОБЕ стороны — иначе переключатель был бы декоративным
        // ровно там, где он нужен: чтобы проверить проигрывающий вариант на живом канале.
        assert_eq!(
            select_congestion_control(None, Fixed(Cubic), LinkCost::Unmetered),
            Cubic
        );
        assert_eq!(
            select_congestion_control(None, Fixed(Bbr), LinkCost::Metered),
            Bbr
        );

        // Отрицательная половина: «авто» обязано вернуть решение правилу, а не запомнить прошлое.
        assert_eq!(
            select_congestion_control(None, Auto, LinkCost::Metered),
            Cubic
        );
        assert_eq!(
            select_congestion_control(None, Auto, LinkCost::Unmetered),
            Bbr
        );
    }

    #[test]
    fn the_env_var_beats_the_switch_so_an_ab_run_measures_what_its_log_says() {
        // Переменную ставит скрипт под конкретный прогон. Если бы залежавшаяся настройка её
        // перебивала, замер молча мерил бы не то, что записано в его же логе.
        assert_eq!(
            select_congestion_control(
                Some("cubic"),
                CongestionPreference::Fixed(CongestionChoice::Bbr),
                LinkCost::Unmetered
            ),
            CongestionChoice::Cubic
        );

        // Отрицательная половина: НЕраспознанная переменная не должна отбирать решение у
        // переключателя — иначе опечатка в скрипте тихо вернула бы всех в автоматику.
        assert_eq!(
            select_congestion_control(
                Some("bbr2"),
                CongestionPreference::Fixed(CongestionChoice::Cubic),
                LinkCost::Unmetered
            ),
            CongestionChoice::Cubic
        );
    }

    #[test]
    fn the_setting_round_trips_by_name_and_refuses_nonsense() {
        for name in ["auto", "bbr", "cubic", "newreno"] {
            let parsed = parse_congestion_preference(name).expect("known name");
            assert_eq!(congestion_preference_name(parsed), name);
        }
        assert_eq!(
            parse_congestion_preference(" BBR "),
            Some(CongestionPreference::Fixed(CongestionChoice::Bbr)),
            "пробелы и регистр не должны ломать сохранённое значение"
        );

        // Отрицательная половина: мусор — это ошибка, а не молчаливое «авто». Иначе опечатка в
        // сохранённом файле выключила бы настройку, и никто бы не заметил.
        assert_eq!(parse_congestion_preference("bbr2"), None);
        assert_eq!(parse_congestion_preference(""), None);
    }

    #[test]
    fn the_process_wide_preference_defaults_to_auto_and_round_trips() {
        assert_eq!(
            congestion_preference(),
            CongestionPreference::Auto,
            "ядро не выбирает контроллер за пользователя, пока его не попросили"
        );
        set_congestion_preference(CongestionPreference::Fixed(CongestionChoice::Cubic));
        assert_eq!(
            congestion_preference(),
            CongestionPreference::Fixed(CongestionChoice::Cubic)
        );
        set_congestion_preference(CongestionPreference::Auto);
        assert_eq!(congestion_preference(), CongestionPreference::Auto);
    }

    #[test]
    fn the_datagram_send_buffer_knob_refuses_a_value_that_would_break_media() {
        // Ручка существует ради A/B на одной сборке: буфер quinn при переполнении молча роняет
        // САМЫЕ СТАРЫЕ датаграммы, и это единственный неизмеренный участок между «мы отправили»
        // и проводом.
        assert_eq!(datagram_send_buffer_from_env(Some("65536")), Some(65_536));
        assert_eq!(datagram_send_buffer_from_env(Some(" 16384 ")), Some(16_384));

        // Отрицательная половина: не задано — не трогаем умолчание quinn; мусор и слишком малое
        // значение тоже не трогают, а не «применяются как есть». Буфер меньше кадра — это не
        // маленький буфер, это выключенное видео.
        assert_eq!(datagram_send_buffer_from_env(None), None);
        assert_eq!(datagram_send_buffer_from_env(Some("")), None);
        assert_eq!(datagram_send_buffer_from_env(Some("много")), None);
        assert_eq!(datagram_send_buffer_from_env(Some("4096")), None);
    }

    #[test]
    fn the_queue_budget_never_changes_which_controller_was_chosen() {
        // Половина решения, которую раньше нельзя было даже сформулировать: включённый бюджет
        // очереди строил свой Cubic и молча отменял выбор BBR/metered. Утверждение пинит саму
        // независимость; от повторения на месте вызова спасает не тест, а то, что контроллер там
        // теперь строится ровно один раз, из этого решения (наружу quinn фабрику не отдаёт).
        for budget_env in [None, Some("25"), Some("10")] {
            for cost in [LinkCost::Unmetered, LinkCost::Unknown] {
                let (choice, _) = congestion_plan(None, CongestionPreference::Auto, cost, budget_env);
                assert_eq!(
                    choice,
                    select_congestion_control(None, CongestionPreference::Auto, cost),
                    "бюджет очереди {budget_env:?} не должен менять выбор контроллера при {cost:?}"
                );
            }
        }

        // Отрицательная половина: сам бюджет при этом обязан доходить до решения, иначе первое
        // утверждение выполнялось бы просто потому, что обёртки никогда нет.
        assert_eq!(
            congestion_plan(None, CongestionPreference::Auto, LinkCost::Unmetered, Some("25")).1,
            QueueBudget::Set(std::time::Duration::from_millis(25)),
            "заданный бюджет обязан попасть в решение"
        );
        assert_eq!(
            congestion_plan(None, CongestionPreference::Auto, LinkCost::Unmetered, None).1,
            QueueBudget::Default,
            "без ключа — бюджет несущей по умолчанию"
        );
    }

    #[test]
    fn the_queue_budget_wraps_the_chosen_controller_instead_of_replacing_it() {
        use quinn::congestion::ControllerFactory;

        const MTU: u16 = 1200;
        let now = std::time::Instant::now();
        let lost = 10 * u64::from(MTU);

        // Наблюдаемая разница между двумя вариантами выбора: Cubic отвечает на потерю сжатием окна,
        // BBR по устройству нечувствителен к ней (ровно поэтому он и опасен против полисера —
        // docs/bugs/uplink-goodput-retransmit.md, P1).
        let mut cubic = controller_factory_for(CongestionChoice::Cubic).build(now, MTU);
        let mut bbr = controller_factory_for(CongestionChoice::Bbr).build(now, MTU);
        let cubic_before = cubic.window();
        cubic.on_congestion_event(now, now, false, lost);
        bbr.on_congestion_event(now, now, false, lost);
        assert!(
            cubic.window() < cubic_before,
            "Cubic обязан сжать окно на потере, иначе признак ниже ничего не различает"
        );
        assert!(
            bbr.window() > cubic.window(),
            "BBR не сжимается на потере так, как Cubic — это и есть наблюдаемый признак выбора"
        );

        // Обёртка обязана сохранить выбранный контроллер, а не подменить его.
        let mut capped = Arc::new(crate::bdp_cap::BdpCappedControllerFactory::new(
            controller_factory_for(CongestionChoice::Bbr),
            Duration::from_millis(25),
        ))
        .build(now, MTU);
        capped.on_congestion_event(now, now, false, lost);
        assert!(
            capped.window() > cubic.window(),
            "под бюджетом очереди должен остаться выбранный контроллер, а не подставленный Cubic"
        );
    }

    #[test]
    fn bbr_is_the_default_and_metered_is_the_only_exception() {
        // Умолчание. Незнание стоимости — это тоже умолчание, и это осознано: на неопознанном
        // домашнем канале Cubic стоил бы нам 18-кратной разницы.
        assert_eq!(
            select_congestion_control(None, CongestionPreference::Auto, LinkCost::Unmetered),
            CongestionChoice::Bbr
        );
        // Незнание разрешается по платформе: на настольной машине неопознанный канал почти
        // всегда фиксированный, на телефоне — сплошь и рядом сотовый. Обе ветки проверяем здесь,
        // чтобы правило для Android не оставалось непроверенным до появления Android-сборки.
        assert_eq!(
            unknown_cost_choice(false),
            CongestionChoice::Bbr,
            "настольная машина: незнание не повод дарить 18-кратную разницу"
        );
        assert_eq!(
            unknown_cost_choice(true),
            CongestionChoice::Cubic,
            "телефон: незнание не повод жечь трафик пользователя"
        );
        assert_eq!(
            select_congestion_control(None, CongestionPreference::Auto, LinkCost::Unknown),
            unknown_cost_choice(cfg!(target_os = "android")),
            "правило обязано звать эту же развилку, а не иметь свою копию"
        );

        // Единственное исключение — и ради него всё правило и заведено.
        assert_eq!(
            select_congestion_control(None, CongestionPreference::Auto, LinkCost::Metered),
            CongestionChoice::Cubic,
            "на метрируемом канале BBR жёг 32-38 % трафика на ретрансмите — это деньги"
        );

        // Тумблер сильнее автоматики НАРОЧНО: им гоняются A/B на одной сборке, и он обязан
        // перебивать в обе стороны, иначе половину замеров нельзя поставить.
        assert_eq!(
            select_congestion_control(Some("cubic"), CongestionPreference::Auto, LinkCost::Unmetered),
            CongestionChoice::Cubic
        );
        assert_eq!(
            select_congestion_control(Some("bbr"), CongestionPreference::Auto, LinkCost::Metered),
            CongestionChoice::Bbr
        );
        assert_eq!(
            select_congestion_control(Some("  NewReno "), CongestionPreference::Auto, LinkCost::Metered),
            CongestionChoice::NewReno,
            "регистр и пробелы не должны решать, чем мы едем"
        );

        // Отрицательная половина: нераспознанное значение НЕ повод молча свалиться в умолчание
        // quinn (а оно Cubic). Раньше именно так и было, и с новым правилом это тихо отменяло бы
        // всё решение.
        assert_eq!(
            select_congestion_control(Some("quiche-bbr2"), CongestionPreference::Auto, LinkCost::Unmetered),
            CongestionChoice::Bbr
        );
        assert_eq!(
            select_congestion_control(Some(""), CongestionPreference::Auto, LinkCost::Metered),
            CongestionChoice::Cubic
        );
    }

    /// Стоимость канала — подаваемый снаружи факт, и по умолчанию мы её НЕ знаем.
    #[test]
    fn link_cost_defaults_to_unknown_and_round_trips() {
        assert_eq!(
            link_cost(),
            LinkCost::Unknown,
            "ядро не выдумывает стоимость канала"
        );
        set_link_cost(LinkCost::Metered);
        assert_eq!(link_cost(), LinkCost::Metered);
        set_link_cost(LinkCost::Unmetered);
        assert_eq!(link_cost(), LinkCost::Unmetered);
        set_link_cost(LinkCost::Unknown);
        assert_eq!(link_cost(), LinkCost::Unknown);
    }

    /// Over TCP a connection holds its queue to the door's budget; over UDP it runs uncapped; a
    /// budget set by hand wins on both.
    #[test]
    fn a_connection_over_tcp_runs_with_the_door_queue_budget() {
        let hand = std::time::Duration::from_millis(10);
        let default = QueueBudget::Default;
        assert_eq!(queue_cap_for(TransportCarrier::Tcp, default), Some(QueueCap::Delay(DOOR_QUEUE_BUDGET)));
        assert_eq!(queue_cap_for(TransportCarrier::Udp, default), Some(QueueCap::Rate(UDP_QUEUE_BUDGET)));
        assert_eq!(queue_cap_for(TransportCarrier::Tcp, QueueBudget::Set(hand)), Some(QueueCap::Delay(hand)));
        assert_eq!(queue_cap_for(TransportCarrier::Udp, QueueBudget::Set(hand)), Some(QueueCap::Rate(hand)));
    }

    #[test]
    fn transport_config_builds() {
        let _config = omni_transport_config();
        // Smoke: построение не паникует.
    }

    #[test]
    fn client_config_builds_with_alpn() {
        let _ = quinn::rustls::crypto::ring::default_provider().install_default();
        let config = omni_client_config(b"omni-test-v1");
        assert!(config.is_ok(), "client config should build: {config:?}");
    }

    #[test]
    fn server_config_builds_with_self_signed_cert() {
        let _ = quinn::rustls::crypto::ring::default_provider().install_default();
        let certified = rcgen::generate_simple_self_signed(vec!["test".to_string()]).unwrap();
        let key = PrivateKeyDer::Pkcs8(certified.signing_key.serialize_der().into());
        let cert = certified.cert.der().clone();
        let config = omni_server_config(cert, key, b"omni-test-v1");
        assert!(config.is_ok(), "server config should build: {config:?}");
    }
}
