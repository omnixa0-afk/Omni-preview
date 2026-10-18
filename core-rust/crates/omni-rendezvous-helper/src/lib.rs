//! The rendezvous helper process — the untrusted-by-design, key-free courier that does the third-
//! party network I/O for the core (privilege separation; see the bootstrap ADR).
//!
//! One process = the network-facing side of rendezvous. It speaks a tiny line protocol over a local
//! socket; the core drives it. It holds NO keys and never seals/opens beacons — those stay in the
//! core. Commands (one request line in → one `OK …` / `ERR …` line out):
//!   * `publish <topic_hex> <blob_hex>`  — publish an opaque sealed blob under a blinded topic to
//!     every provider (N-of-many). Reply `OK <accepted-provider-count>`.
//!   * `fetch <topic_hex>`               — fetch opaque blobs for a topic from every provider.
//!     Reply `OK <blob_hex> <blob_hex> …` (empty `OK` = nothing yet).
//!   (STUN is NOT here: it must leave from the mesh's own QUIC socket to describe a dialable
//!   port — see `omni-network::stun_socket`.)
//!   * `quit` — close this connection.
//!
//! The provider connections (and, later, a named-pipe transport + per-component proxy) live here so
//! a compromise of this process is contained to "a malicious courier" = DoS only — it can't deceive
//! (identity-bound trust + the AEAD beacon) and can't reach the core's keys.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use omni_rendezvous::{courier_label, ChannelError, ProviderHealth, RendezvousChannel, Topic};
use omni_rendezvous_providers::{BtDhtChannel, NostrChannel, NtfyChannel};

/// A provider paired with its circuit-breaker health, so the N-of-many fan-out can SKIP a provider
/// that is rate-limited/down and keep working through the healthy ones (see [`fan_out`]).
///
/// A breakdown (timeout, unreachable, refused) is one breaker for everything: a courier that is down
/// is down for reading too, and trying it again costs the round its timeout. A QUOTA is per kind of
/// operation: ntfy.sh meters published messages by the day, while reading and subscribing stay free —
/// so a 429 on publishing used to park the courier for an hour for reading as well, and we stopped
/// finding beacons that were right there. A subscription has its own too: ntfy also caps how many
/// subscriptions one IP holds open, and hitting that must not stop a plain fetch.
struct GatedProvider {
    /// A compact human label for the per-courier trace the core logs (`ntfy:ntfy.sh`,
    /// `nostr:offchain.pub`, `dht:mainline`) — the courier KIND plus its host, from the URL scheme.
    label: String,
    channel: Box<dyn RendezvousChannel + Send + Sync>,
    /// The URL, when this courier is one of [`omni_rendezvous::NOSTR_RELAY_POOL`]: a pool relay serves
    /// only the topics [`omni_rendezvous::pool_rank`] gives it. `None` — every topic.
    pool_url: Option<String>,
    /// The BT-DHT courier: slow (seconds per operation) and without a subscription, so the core reads
    /// it apart from the web couriers (see [`Legs`]).
    is_dht: bool,
    /// The user set this courier to "only read": we look for contacts there, post nothing.
    read_only: bool,
    /// Not one of ours — a courier a device we talk to uses: used for a topic only when the core names it.
    pair_only: bool,
    /// The courier's URL, as the core names it for a topic.
    url: String,
    health: Mutex<ProviderHealth>,
    publish_quota: Mutex<ProviderHealth>,
    read_quota: Mutex<ProviderHealth>,
    watch_quota: Mutex<ProviderHealth>,
}

/// Which couriers a fetch asks: the web ones (ntfy, Nostr — fast), the BT-DHT one (seconds per query),
/// or all. The core asks the DHT only when the web couriers did not have what it looks for, so a
/// fetch round no longer waits for the slowest courier every time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Legs {
    Web,
    Dht,
    All,
    /// One web kind only — for a topic that kind keeps for itself (the relay directory's shelves).
    Nostr,
    Ntfy,
}

impl Legs {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "web" => Some(Self::Web),
            "dht" => Some(Self::Dht),
            "all" => Some(Self::All),
            "nostr" => Some(Self::Nostr),
            "ntfy" => Some(Self::Ntfy),
            _ => None,
        }
    }

    fn includes(self, provider: &GatedProvider) -> bool {
        match self {
            Self::Web => !provider.is_dht,
            Self::Dht => provider.is_dht,
            Self::All => true,
            Self::Nostr => courier_kind(provider) == "nostr",
            Self::Ntfy => courier_kind(provider) == "ntfy",
        }
    }
}

/// Which quota an operation draws on: publishing, fetching, or holding a subscription.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Publish,
    Read,
    Watch,
}

impl GatedProvider {
    fn new(label: String, channel: Box<dyn RendezvousChannel + Send + Sync>) -> Self {
        Self {
            label,
            channel,
            pool_url: None,
            is_dht: false,
            read_only: false,
            pair_only: false,
            url: String::new(),
            health: Mutex::new(ProviderHealth::new()),
            publish_quota: Mutex::new(ProviderHealth::new()),
            read_quota: Mutex::new(ProviderHealth::new()),
            watch_quota: Mutex::new(ProviderHealth::new()),
        }
    }

    /// The courier for a set entry (a URL, marked when it is only read).
    fn for_entry(entry: &str, channel: Box<dyn RendezvousChannel + Send + Sync>) -> Self {
        let (url, courier_use) = omni_rendezvous::parse_courier_entry(entry);
        let mut provider = Self::for_url(url, channel);
        provider.read_only = courier_use == omni_rendezvous::CourierUse::ReadOnly;
        provider.pair_only = courier_use == omni_rendezvous::CourierUse::PairOnly;
        provider
    }

    /// The courier for `url`, a pool relay when the URL is in the pool.
    fn for_url(url: &str, channel: Box<dyn RendezvousChannel + Send + Sync>) -> Self {
        let mut provider = Self::new(courier_label(url), channel);
        provider.url = url.to_string();
        provider.is_dht = url.starts_with("dht://");
        if omni_rendezvous::NOSTR_RELAY_POOL.contains(&url) {
            provider.pool_url = Some(url.to_string());
        }
        provider
    }

    fn quota(&self, op: Op) -> &Mutex<ProviderHealth> {
        match op {
            Op::Publish => &self.publish_quota,
            Op::Read => &self.read_quota,
            Op::Watch => &self.watch_quota,
        }
    }

    fn is_cooling_down(&self, op: Op, now: u64) -> bool {
        health(&self.health).is_cooling_down(now) || health(self.quota(op)).is_cooling_down(now)
    }

    /// When `op` may be tried again (`0` = now).
    fn ready_at(&self, op: Op) -> u64 {
        let broken = health(&self.health).cooldown_until_ms().unwrap_or(0);
        let quota = health(self.quota(op)).cooldown_until_ms().unwrap_or(0);
        broken.max(quota)
    }

    fn on_success(&self, op: Op) {
        health(&self.health).on_success();
        health(self.quota(op)).on_success();
    }

    fn on_quota_exhausted(&self, op: Op, now: u64) {
        health(self.quota(op)).on_quota_exhausted(now);
    }

    fn on_failure(&self, now: u64) {
        health(&self.health).on_failure(now);
    }
}

type Providers = Arc<Vec<GatedProvider>>;

/// The couriers `topic` goes through for `op`. `among`: the couriers the core allows for this topic —
/// a pair's: post where the other device looks, look where it posts — or `None` for our own set
/// (every courier of ours, none of the pair-only ones). Then: every allowed courier outside the pool, and of the pool relays
/// the first [`omni_rendezvous::POOL_RELAYS_TO_PUBLISH`] (publishing) or
/// [`omni_rendezvous::POOL_RELAYS_TO_READ`] (reading, watching) by [`omni_rendezvous::pool_rank`] that
/// are not cooling down — or, when too few are up, the best-ranked ones anyway, for `fan_out` to skip or
/// probe as it does any cooling courier.
fn couriers_for_topic(
    providers: &Providers,
    topic: &Topic,
    op: Op,
    now: u64,
    among: Option<&[String]>,
) -> Vec<usize> {
    let wanted = match op {
        Op::Publish => omni_rendezvous::POOL_RELAYS_TO_PUBLISH,
        Op::Read | Op::Watch => omni_rendezvous::POOL_RELAYS_TO_READ,
    };
    let allowed = |p: &GatedProvider| match among {
        Some(urls) => urls.contains(&p.url),
        None => !p.pair_only,
    };
    let mut pool: Vec<(usize, [u8; 32])> = providers
        .iter()
        .enumerate()
        .filter(|(_, p)| allowed(p))
        .filter(|(_, p)| op != Op::Publish || !p.read_only)
        .filter_map(|(i, p)| p.pool_url.as_deref().map(|url| (i, omni_rendezvous::pool_rank(topic, url))))
        .collect();
    pool.sort_by(|a, b| a.1.cmp(&b.1));
    let mut chosen: Vec<usize> = pool
        .iter()
        .map(|(i, _)| *i)
        .filter(|i| !providers[*i].is_cooling_down(op, now))
        .take(wanted)
        .collect();
    for (i, _) in &pool {
        if chosen.len() >= wanted.min(pool.len()) {
            break;
        }
        if !chosen.contains(i) {
            chosen.push(*i);
        }
    }
    let mut eligible: Vec<usize> = (0..providers.len())
        .filter(|i| allowed(&providers[*i]))
        .filter(|i| providers[*i].pool_url.is_none() || chosen.contains(i))
        .filter(|i| op != Op::Publish || !providers[*i].read_only)
        .collect();
    eligible.sort_unstable();
    eligible
}

/// One provider's outcome for a single fan-out round — so the core can log WHICH courier carried (or
/// dropped) a publish/fetch instead of a bare success count. A `Skipped` provider was in cooldown and
/// not contacted this round; `ms` is the probe round-trip for the others.
enum CourierResult {
    /// The call succeeded with a meaningful result (publish accepted / fetch returned ≥1 blob).
    Hit,
    /// The call succeeded but produced nothing (fetch found no blobs). Publish never emits this.
    Miss,
    /// The call errored (the breaker grew this provider's cooldown).
    Fail,
    /// The provider was cooling down and was not contacted this round.
    Skipped,
    /// Провайдер жив и ответил, но у нас кончилась квота на нём (HTTP 429 и подобное).
    QuotaExhausted,
}

struct CourierOutcome {
    label: String,
    result: CourierResult,
    ms: u64,
    /// Короткая причина отказа — то, что сказал сам провайдер. Пусто при успехе.
    ///
    /// Без неё `=fail` означает «что-то пошло не так» и каждый разбор начинается с нуля: сегодня
    /// на выяснение, что ntfy отвечает 429 по суточной квоте, ушёл отдельный заход
    /// (docs/bugs/ntfy-courier-out-of-quota.md). Пробелы схлопнуты — атрибуция разбирается по
    /// пробелам, строка обязана остаться однословной на запись.
    reason: String,
}

/// Ужать сообщение провайдера до одного токена, пригодного для атрибуции.
fn short_reason(error: &ChannelError) -> String {
    let compact: String = error
        .0
        .chars()
        .map(|c| if c.is_whitespace() { '_' } else { c })
        .take(80)
        .collect();
    compact.trim_matches('_').to_string()
}

/// Render the per-provider outcomes as a compact, space-separated attribution the core appends to its
/// trace, e.g. `ntfy:ntfy.sh=ok+412ms nostr:offchain.pub=empty+180ms dht:mainline=skip`.
fn attribution(reports: &[CourierOutcome]) -> String {
    reports
        .iter()
        .map(|r| match r.result {
            CourierResult::Hit => format!("{}=ok+{}ms", r.label, r.ms),
            CourierResult::Miss => format!("{}=empty+{}ms", r.label, r.ms),
            CourierResult::Fail if r.reason.is_empty() => format!("{}=fail+{}ms", r.label, r.ms),
            CourierResult::Fail => format!("{}=fail+{}ms:{}", r.label, r.ms, r.reason),
            CourierResult::Skipped => format!("{}=skip", r.label),
            CourierResult::QuotaExhausted if r.reason.is_empty() => {
                format!("{}=quota+{}ms", r.label, r.ms)
            }
            CourierResult::QuotaExhausted => format!("{}=quota+{}ms:{}", r.label, r.ms, r.reason),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Wall-clock millis for the health breaker. The breaker only needs relative deltas (cooldown
/// windows), so a one-off clock blip just nudges a retry slightly.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Lock a health mutex, recovering from poisoning — a panicked sibling thread must not wedge the
/// fan-out; the breaker state is advisory.
fn health(m: &Mutex<ProviderHealth>) -> std::sync::MutexGuard<'_, ProviderHealth> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Run `op` across the provider set with circuit-breaker gating (N-of-many), returning the successful
/// results. A provider currently in backoff cooldown is SKIPPED — a rate-limited/down provider must
/// not consume the round (re-POSTing into a 429 only prolongs it), so the effort goes to the healthy
/// ones. Each outcome updates that provider's health: success resets the breaker, an error grows its
/// cooldown ([`ProviderHealth`]). If EVERY provider is cooling down, the one whose cooldown expires
/// soonest is still probed — so a transient all-provider hiccup never makes rendezvous go fully dark.
///
/// Alongside the successful results it returns a per-provider [`CourierOutcome`] for EVERY provider
/// (including the ones skipped for cooldown), so the caller can log which courier carried the round.
/// `is_meaningful` classifies an `Ok` value as a hit vs an empty miss (publish passes `|_| true`;
/// fetch passes "returned ≥1 blob") — it only shapes the log, never the returned values.
#[cfg(test)]
fn fan_out<T: Send>(
    providers: &Providers,
    op_kind: Op,
    now: u64,
    op: impl Fn(&dyn RendezvousChannel) -> Result<T, ChannelError> + Sync,
    is_meaningful: impl Fn(&T) -> bool,
) -> (Vec<T>, Vec<CourierOutcome>) {
    let all: Vec<usize> = (0..providers.len()).collect();
    fan_out_among(providers, &all, op_kind, now, op, is_meaningful)
}

/// [`fan_out`] over the couriers `eligible` (indices into `providers`) — the ones a topic goes through
/// (see [`couriers_for_topic`]). The others are not tried and not reported.
fn fan_out_among<T: Send>(
    providers: &Providers,
    eligible: &[usize],
    op_kind: Op,
    now: u64,
    op: impl Fn(&dyn RendezvousChannel) -> Result<T, ChannelError> + Sync,
    is_meaningful: impl Fn(&T) -> bool,
) -> (Vec<T>, Vec<CourierOutcome>) {
    fan_out_among_indexed(providers, eligible, op_kind, now, |_, ch| op(ch), is_meaningful)
}

/// [`fan_out_among`] whose `op` also gets the courier's index — for work that differs per courier.
fn fan_out_among_indexed<T: Send>(
    providers: &Providers,
    eligible: &[usize],
    op_kind: Op,
    now: u64,
    op: impl Fn(usize, &dyn RendezvousChannel) -> Result<T, ChannelError> + Sync,
    is_meaningful: impl Fn(&T) -> bool,
) -> (Vec<T>, Vec<CourierOutcome>) {
    let mut probe: Vec<usize> = eligible
        .iter()
        .copied()
        .filter(|i| !providers[*i].is_cooling_down(op_kind, now))
        .collect();
    if probe.is_empty() {
        if let Some(i) = soonest_cooldown_index(providers, eligible, op_kind) {
            probe.push(i);
        }
    }
    // Курьеры опрашиваются ПАРАЛЛЕЛЬНО, и это не оптимизация, а условие осмысленности N-of-many.
    // Последовательный обход стоил СУММУ задержек всех провайдеров, поэтому самый медленный
    // диктовал скорость всем: живой замер 2026-08-13 на двухмашинном стенде — ntfy ~0.5 с,
    // nostr ~1 с, BT-DHT 3-10 с (итеративный поиск по рою — его нормальная цена), итого раунд ~6 с
    // вместо ~1 с. Хуже того, КАЖДЫЙ добавленный релей делал рандеву медленнее — избыточность
    // приходилось покупать задержкой, хотя весь смысл в том, что достаточно ОДНОГО ответа.
    //
    // Провайдеры независимы: свой канал (`Send + Sync`), свой мьютекс здоровья, общего изменяемого
    // состояния нет. Поэтому хватает `std::thread::scope` без единой новой зависимости. Порядок
    // отчётов восстанавливается по индексу — строка атрибуции обязана оставаться детерминированной.
    type Slot<T> = Option<(CourierResult, u64, Option<T>, String)>;
    let mut slots: Vec<Slot<T>> = (0..providers.len()).map(|_| None).collect();
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(probe.len());
        for &i in &probe {
            let provider = &providers[i];
            let op = &op;
            handles.push((
                i,
                scope.spawn(move || {
                    let started = Instant::now();
                    let outcome = op(i, provider.channel.as_ref());
                    (outcome, started.elapsed().as_millis() as u64)
                }),
            ));
        }
        for (i, handle) in handles {
            let provider = &providers[i];
            // Паника внутри одного курьера не должна валить раунд — остальные ноги уже отработали.
            let Ok((outcome, ms)) = handle.join() else {
                provider.on_failure(now);
                slots[i] = Some((CourierResult::Fail, 0, None, "panicked".to_string()));
                continue;
            };
            slots[i] = Some(match outcome {
                Ok(value) => {
                    provider.on_success(op_kind);
                    let result = if is_meaningful(&value) {
                        CourierResult::Hit
                    } else {
                        CourierResult::Miss
                    };
                    (result, ms, Some(value), String::new())
                }
                Err(error) if error.is_quota_exhausted() => {
                    // Квота, а не поломка: провайдер жив и отвечает, просто до сброса лимита нам
                    // тут ничего не светит. Отдельная метка в трассе — чтобы `=fail` не читалось
                    // как «курьер сломан», и отдельный длинный откат вместо долбёжки в дверь.
                    let reason = short_reason(&error);
                    provider.on_quota_exhausted(op_kind, now);
                    (CourierResult::QuotaExhausted, ms, None, reason)
                }
                Err(error) => {
                    let reason = short_reason(&error);
                    provider.on_failure(now);
                    (CourierResult::Fail, ms, None, reason)
                }
            });
        }
    });

    let mut out = Vec::new();
    let mut reports = Vec::with_capacity(eligible.len());
    for (i, (p, slot)) in providers.iter().zip(slots).enumerate() {
        if !eligible.contains(&i) {
            continue;
        }
        match slot {
            // Провайдер в откате в этом раунде не опрашивался — но в отчёте он есть, чтобы в логе
            // был виден ВЕСЬ набор курьеров, а не только опрошенные.
            None => reports.push(CourierOutcome {
                label: p.label.clone(),
                result: CourierResult::Skipped,
                ms: 0,
                reason: String::new(),
            }),
            Some((result, ms, value, reason)) => {
                reports.push(CourierOutcome { label: p.label.clone(), result, ms, reason });
                if let Some(value) = value {
                    out.push(value);
                }
            }
        }
    }
    (out, reports)
}

/// Index of the provider whose cooldown expires soonest (a ready provider — `None` cooldown — sorts
/// first). `None` only for an empty set. Used as the never-go-dark probe when all are cooling down.
fn soonest_cooldown_index(providers: &Providers, among: &[usize], op: Op) -> Option<usize> {
    among.iter().copied().min_by_key(|i| providers[*i].ready_at(op))
}

/// Run the courier with `args` (everything AFTER the program name) and return the process exit code.
///
/// Two callers, one body: the thin `omni-rendezvous-helper` bin, and the single-file host — which has
/// no sibling helper .exe on disk, so it re-execs ITSELF with `--run-helper rendezvous …` and routes
/// here through the FFI (`omni_run_helper`). Never calls `process::exit`: the FFI caller must get the
/// code back rather than have the host process torn down.
/// См. docs/decisions/serverless-bootstrap-rendezvous.md §Architecture.
pub fn run(args: Vec<String>) -> i32 {
    let mut args = args.into_iter();
    let mut listen: Option<String> = None;
    let mut connect: Option<String> = None;
    let mut connect_cmd = String::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = args.next(),
            "--connect" => {
                connect = args.next();
                connect_cmd = args.by_ref().collect::<Vec<_>>().join(" ");
                break;
            }
            _ => {}
        }
    }

    if let Some(endpoint) = connect {
        return run_client(&endpoint, &connect_cmd);
    }

    let Some(addr) = listen else {
        eprintln!("usage: omni-rendezvous-helper --listen <addr>  |  --connect <addr> <command>");
        return 2;
    };

    // Service (listener) mode must never show a terminal window: detach any inherited console so
    // even a careless launch can't pop one. (Client mode keeps its console for stdout.) The app
    // should additionally spawn us with CreateNoWindow so no console is created in the first place.
    #[cfg(windows)]
    detach_console();

    // The provider set (N-of-many): publish/fetch fan OUT across all of them (see `handle_publish`/
    // `handle_fetch`), so one provider being rate-limited or down can't sink rendezvous —
    // squarely on the availability axis (a adversary must block them ALL). The set is operator/user-
    // configurable via `OMNI_RENDEZVOUS_PROVIDERS` (comma-separated URLs, addable like relays). The URL
    // SCHEME picks the courier: `http(s)://` = ntfy HTTP pub/sub, `ws(s)://` = a Nostr relay — two
    // DIFFERENT protocols by default, so one host going down can't take out the rendezvous plane.
    let providers: Providers = Arc::new(
        omni_rendezvous::parse_provider_urls(std::env::var("OMNI_RENDEZVOUS_PROVIDERS").ok())
            .into_iter()
            .filter_map(|entry| {
                let (url, _) = omni_rendezvous::parse_courier_entry(&entry);
                channel_for_url(url).map(|channel| GatedProvider::for_entry(&entry, channel))
            })
            .collect(),
    );

    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(error) => {
            eprintln!("cannot listen on {addr}: {error}");
            return 1;
        }
    };
    // Print the RESOLVED bound address (not the arg) so a parent that passed `:0` can read
    // the ephemeral port from this line. Mirrors omni-node.
    let bound = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| addr.clone());
    eprintln!("omni-rendezvous-helper listening on {bound}");
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let providers = Arc::clone(&providers);
        std::thread::spawn(move || serve(stream, &providers));
    }
    0
}

fn serve(stream: TcpStream, providers: &Providers) {
    let Ok(read_half) = stream.try_clone() else { return };
    let mut reader = BufReader::new(read_half);
    let mut writer = stream;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "quit" {
            let _ = writeln!(writer, "OK bye");
            break;
        }
        let response = handle_line(trimmed, providers);
        if writeln!(writer, "{response}").is_err() {
            break;
        }
        let _ = writer.flush();
    }
}

/// One command line → its reply. Every argument is a whitespace-separated token (hex, a number, a
/// word). It used to be `splitn(4, ' ')`, sized for `publish <topic> <key> <blob>`: when `publish` got
/// its fourth and fifth argument (the lifetime, the priority), they stuck to the blob, and every
/// publish was refused as malformed — while the tests, calling `handle_publish` directly, passed.
fn handle_line(line: &str, providers: &Providers) -> String {
    let mut parts = line.split_whitespace();
    match parts.next().unwrap_or("") {
        "publish" => handle_publish(
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            providers,
        ),
        "fetch" => handle_fetch(parts.next(), parts.next(), parts.next(), parts.next(), providers),
        "fetch-many" => handle_fetch_many(parts.next(), parts.next(), providers),
        "watch" => handle_watch(line.trim_start_matches("watch"), providers),
        other => format!("ERR unknown command '{other}'"),
    }
}

fn handle_publish(
    topic_hex: Option<&str>,
    dhtkey_hex: Option<&str>,
    blob_hex: Option<&str>,
    ttl_secs: Option<&str>,
    priority: Option<&str>,
    among: Option<&str>,
    slim_hex: Option<&str>,
    spread: Option<&str>,
    replace: Option<&str>,
    providers: &Providers,
) -> String {
    let (Some(topic_hex), Some(dhtkey_hex), Some(blob_hex), Some(ttl_secs), Some(priority), Some(among)) =
        (topic_hex, dhtkey_hex, blob_hex, ttl_secs, priority, among)
    else {
        return "ERR publish needs <topic_hex> <dhtkey_hex> <blob_hex> <ttl_secs> <contact|background> <couriers|-> [slim_hex|-] [all|<n>:<kind+kind,kind,…>] [keep|replace]"
            .to_string();
    };
    let Some(spread) = Spread::parse(spread.unwrap_or("all")) else {
        return "ERR bad spread".to_string();
    };
    // A beacon is this writer's current value (`replace`): a courier that can drops the previous one.
    // A note is `keep` — its earlier copies must stay (RendezvousChannel::publish_replacing).
    let replace = match replace.unwrap_or("keep") {
        "keep" => false,
        "replace" => true,
        _ => return "ERR bad replace".to_string(),
    };
    // The same beacon without its relay bundle, for a courier the full one does not fit (a BT-DHT
    // record — `RendezvousChannel::max_blob`). Optional: most beacons fit everywhere.
    let slim = match slim_hex {
        None | Some("-") => None,
        Some(hex) => match from_hex(hex) {
            Some(slim) => Some(slim),
            None => return "ERR bad slim blob hex".to_string(),
        },
    };
    let among = parse_among(among);
    // A background post (a find-by-ID beacon, a mailbox note) leaves a metered courier's last few
    // messages to the beacons a contact is waiting for.
    let background = match priority {
        "contact" => false,
        "background" => true,
        _ => return "ERR bad priority".to_string(),
    };
    let Ok(ttl_secs) = ttl_secs.parse::<u32>() else {
        return "ERR bad ttl".to_string();
    };
    let Some(topic) = Topic::from_hex(topic_hex) else {
        return "ERR bad topic hex".to_string();
    };
    // The per-bucket BT-DHT seed (the core derived it from the pair secret); web couriers ignore it,
    // BT-DHT keys its slots by it. Malformed/short ⇒ `None`, so the BT-DHT leg fails best-effort.
    let dht_key = parse_dht_key(dhtkey_hex);
    let Some(blob) = from_hex(blob_hex) else {
        return "ERR bad blob hex".to_string();
    };
    // Publish is meaningful whenever the courier accepted it (`Ok`) — there is no "empty" publish.
    let now = now_ms();
    let eligible: Vec<usize> = couriers_for_topic(providers, &topic, Op::Publish, now, among.as_deref())
        .into_iter()
        .filter(|i| !providers[*i].read_only)
        .filter(|i| !(background && providers[*i].channel.publish_allowance_low()))
        // A courier neither the blob nor its slim copy fits is not asked at all: its refusal used to
        // count as a failure and put it in cooldown — a mailbox note (~1 KB) took the BT-DHT out for
        // the beacons right after it.
        .filter(|i| {
            providers[*i].channel.max_blob().is_none_or(|max| blob.len() <= max || slim.as_ref().is_some_and(|slim| slim.len() <= max))
        })
        .collect();
    let post = |ch: &dyn RendezvousChannel| {
        let fits = |len: usize| ch.max_blob().is_none_or(|max| len <= max);
        let to_post = if fits(blob.len()) { &blob } else { slim.as_ref().filter(|slim| fits(slim.len())).unwrap_or(&blob) };
        if replace {
            ch.publish_replacing(&topic, dht_key.as_ref(), to_post, ttl_secs)
        } else {
            ch.publish(&topic, dht_key.as_ref(), to_post, ttl_secs)
        }
    };
    let (accepted, reports) = match &spread {
        Spread::All => fan_out_among(providers, &eligible, Op::Publish, now, post, |_| true),
        Spread::Stages { reach, stages } => publish_along_stages(providers, &eligible, stages, *reach, now, post),
    };
    // `OK <accepted_count>\t<per-courier attribution>` — the TAB lets the core split the count it
    // already parsed from the new courier detail without disturbing the old format.
    format!("OK {}\t{}", accepted.len(), attribution(&reports))
}

/// Where one publish goes: every eligible courier (`all`), or along the steps of the publication's
/// plan (`<n>:nostr+dht,ntfy` — docs/decisions/public-relay-directory.md §10): the kinds of one step
/// at once, the next step only if no kind of this one took it, until `n` steps took it. Readers look
/// everywhere, so a beacon in one place is found; the core asks for more steps for a pair whose device
/// is up but does not come (`BeaconPublishLog::note_peer_seen_unlinked`). A kind in no step is not
/// posted to at all (the editor's «Не использовать»; never the BT-DHT for a name beacon).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Spread {
    All,
    Stages { reach: usize, stages: Vec<Vec<String>> },
}

impl Spread {
    fn parse(value: &str) -> Option<Self> {
        if value == "all" || value == "-" {
            return Some(Self::All);
        }
        let (reach, stages) = value.split_once(':')?;
        let reach = reach.parse::<usize>().ok().filter(|n| *n > 0)?;
        let stages: Vec<Vec<String>> = stages
            .split(',')
            .filter(|stage| !stage.is_empty())
            .map(|stage| stage.split('+').filter(|kind| !kind.is_empty()).map(str::to_string).collect())
            .collect();
        (!stages.is_empty() && stages.iter().all(|stage: &Vec<String>| !stage.is_empty()))
            .then_some(Self::Stages { reach, stages })
    }
}

/// The kind of a courier: `nostr`, `ntfy`, `dht` (the label's prefix, see `courier_label`).
fn courier_kind(provider: &GatedProvider) -> &str {
    provider.label.split(':').next().unwrap_or("")
}

/// Publish along the steps: one step at a time (all couriers of its kinds at once — a pair's two pool
/// relays are one kind, Nostr and Mainline in one step are two), until `reach` steps took it. A step
/// with every courier cooling down (out of quota, broken) is passed over — "закончился → следующий".
/// Couriers never asked are not reported (they did nothing; their status stays what the last read
/// said). If no step took it and some were passed over, the soonest-ready of them is still tried, as
/// everywhere — publishing never goes fully dark.
fn publish_along_stages(
    providers: &Providers,
    eligible: &[usize],
    stages: &[Vec<String>],
    reach: usize,
    now: u64,
    post: impl Fn(&dyn RendezvousChannel) -> Result<(), ChannelError> + Sync,
) -> (Vec<()>, Vec<CourierOutcome>) {
    let groups: Vec<Vec<usize>> = stages
        .iter()
        .map(|kinds| {
            eligible.iter().copied().filter(|i| kinds.iter().any(|kind| kind == courier_kind(&providers[*i]))).collect()
        })
        .collect();
    let kinds = reach;

    let mut accepted = Vec::new();
    let mut reports = Vec::new();
    let mut taken = 0;
    let mut passed_over: Vec<usize> = Vec::new();
    for group in groups.into_iter().filter(|g| !g.is_empty()) {
        if taken >= kinds {
            break;
        }
        if group.iter().all(|i| providers[*i].is_cooling_down(Op::Publish, now)) {
            passed_over.extend(group);
            continue;
        }
        let (ok, mut outcomes) = fan_out_among(providers, &group, Op::Publish, now, &post, |_| true);
        if !ok.is_empty() {
            taken += 1;
        }
        accepted.extend(ok);
        reports.append(&mut outcomes);
    }
    if taken == 0 && !passed_over.is_empty() {
        let (ok, mut outcomes) = fan_out_among(providers, &passed_over, Op::Publish, now, &post, |_| true);
        accepted.extend(ok);
        reports.append(&mut outcomes);
    }
    (accepted, reports)
}

fn handle_fetch(
    topic_hex: Option<&str>,
    dhtkey_hex: Option<&str>,
    legs: Option<&str>,
    among: Option<&str>,
    providers: &Providers,
) -> String {
    let (Some(topic_hex), Some(dhtkey_hex), Some(legs), Some(among)) = (topic_hex, dhtkey_hex, legs, among) else {
        return "ERR fetch needs <topic_hex> <dhtkey_hex> <web|dht|all|nostr|ntfy> <couriers|->".to_string();
    };
    let among = parse_among(among);
    let Some(legs) = Legs::parse(legs) else {
        return "ERR bad legs".to_string();
    };
    let Some(topic) = Topic::from_hex(topic_hex) else {
        return "ERR bad topic hex".to_string();
    };
    let dht_key = parse_dht_key(dhtkey_hex);
    // A fetch is a "hit" for a courier when it returned ≥1 blob, an "empty" miss otherwise — so the
    // searcher's log shows WHICH courier actually had the beacon, not just that it was reachable.
    let now = now_ms();
    let eligible: Vec<usize> = couriers_for_topic(providers, &topic, Op::Read, now, among.as_deref())
        .into_iter()
        .filter(|i| legs.includes(&providers[*i]))
        .collect();
    let (results, reports) = fan_out_among(
        providers,
        &eligible,
        Op::Read,
        now,
        |ch| ch.fetch(&topic, dht_key.as_ref()),
        |blobs: &Vec<Vec<u8>>| !blobs.is_empty(),
    );
    let hexes: Vec<String> = results.into_iter().flatten().map(|blob| to_hex(&blob)).collect();
    let attr = attribution(&reports);
    // `OK[ <blob> …]\t<per-courier attribution>` — the TAB separates the blobs (every whitespace token
    // before it) from the courier detail, so the core's blob split is unaffected.
    if hexes.is_empty() {
        format!("OK\t{attr}")
    } else {
        format!("OK {}\t{attr}", hexes.join(" "))
    }
}

/// `fetch-many <legs> <topic_hex>[,<topic_hex>…]` → `OK[ <blob_hex> …]\t<attribution>`: every blob under
/// any of the topics (a set read as one — the relay directory's shelves). Each topic still goes to
/// its own couriers (a pool relay is ranked per topic, and the writer posted where its topic ranks);
/// each courier is asked once, for all of its topics ([`RendezvousChannel::fetch_many`]).
fn handle_fetch_many(legs: Option<&str>, topics: Option<&str>, providers: &Providers) -> String {
    let (Some(legs), Some(topics)) = (legs, topics) else {
        return "ERR fetch-many needs <web|dht|all|nostr|ntfy> <topic_hex>[,<topic_hex>...]".to_string();
    };
    let Some(legs) = Legs::parse(legs) else {
        return "ERR bad legs".to_string();
    };
    let Some(topics) = topics.split(',').map(Topic::from_hex).collect::<Option<Vec<Topic>>>() else {
        return "ERR bad topic hex".to_string();
    };
    let now = now_ms();
    let mut per_courier: Vec<Vec<Topic>> = vec![Vec::new(); providers.len()];
    for topic in &topics {
        for i in couriers_for_topic(providers, topic, Op::Read, now, None) {
            if legs.includes(&providers[i]) {
                per_courier[i].push(*topic);
            }
        }
    }
    let eligible: Vec<usize> = (0..providers.len()).filter(|i| !per_courier[*i].is_empty()).collect();
    let (results, reports) = fan_out_among_indexed(
        providers,
        &eligible,
        Op::Read,
        now,
        |i, ch| ch.fetch_many(&per_courier[i]),
        |blobs: &Vec<Vec<u8>>| !blobs.is_empty(),
    );
    let hexes: Vec<String> = results.into_iter().flatten().map(|blob| to_hex(&blob)).collect();
    let attr = attribution(&reports);
    if hexes.is_empty() {
        format!("OK\t{attr}")
    } else {
        format!("OK {}\t{attr}", hexes.join(" "))
    }
}

/// The couriers the core allows for a topic: comma-separated URLs, or `-` for our own set.
fn parse_among(value: &str) -> Option<Vec<String>> {
    (value != "-").then(|| value.split(',').filter(|url| !url.is_empty()).map(str::to_string).collect())
}

/// The longest single `watch` the helper holds (the core renews it).
const MAX_WATCH_SECS: u64 = 300;

/// `watch <wait_secs> <topic_hex>…` → `OK[ <blob_hex>]\t<attribution>`: hold a subscription on every
/// courier that has one ([`RendezvousChannel::supports_watch`]) and answer on the FIRST arrival — not
/// after the slowest courier, unlike `fetch`: a push is worth exactly as much as its earliest copy.
/// Couriers still waiting when the answer goes out finish on their own (each is bounded by the wait).
/// Empty `OK` on timeout, or at once when no courier can watch.
fn handle_watch(args: &str, providers: &Providers) -> String {
    let mut parts = args.split_whitespace();
    let Some(wait_secs) = parts.next().and_then(|w| w.parse::<u64>().ok()) else {
        return "ERR watch needs <wait_secs> <topic_hex>...".to_string();
    };
    let wait = Duration::from_secs(wait_secs.clamp(1, MAX_WATCH_SECS));
    // `<topic_hex>` (our own set) or `<topic_hex>@<url>,<url>` (a pair's couriers).
    let Some(topics) = parts
        .map(|token| match token.split_once('@') {
            Some((topic, among)) => Topic::from_hex(topic).map(|topic| (topic, parse_among(among))),
            None => Topic::from_hex(token).map(|topic| (topic, None)),
        })
        .collect::<Option<Vec<(Topic, Option<Vec<String>>)>>>()
    else {
        return "ERR bad topic hex".to_string();
    };
    if topics.is_empty() {
        return "ERR watch needs <wait_secs> <topic_hex>...".to_string();
    }
    let now = now_ms();
    // Each courier holds a subscription on the topics it serves (a pool relay — on its share only).
    let mut watched: Vec<(usize, Vec<Topic>)> = Vec::new();
    for (topic, among) in &topics {
        for i in couriers_for_topic(providers, topic, Op::Watch, now, among.as_deref()) {
            let p = &providers[i];
            if !p.channel.supports_watch() || p.is_cooling_down(Op::Watch, now) {
                continue;
            }
            match watched.iter_mut().find(|(j, _)| *j == i) {
                Some((_, mine)) => mine.push(*topic),
                None => watched.push((i, vec![*topic])),
            }
        }
    }
    if watched.is_empty() {
        return "OK\tnone-can-watch".to_string();
    }
    let watchers: Vec<usize> = watched.iter().map(|(i, _)| *i).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    for (i, topics) in watched {
        let (providers, tx) = (Arc::clone(providers), tx.clone());
        std::thread::spawn(move || {
            let started = Instant::now();
            let outcome = providers[i].channel.watch(&topics, wait);
            match &outcome {
                Ok(_) => providers[i].on_success(Op::Watch),
                Err(error) if error.is_quota_exhausted() => {
                    providers[i].on_quota_exhausted(Op::Watch, now_ms())
                }
                Err(_) => providers[i].on_failure(now_ms()),
            }
            let _ = tx.send((i, outcome, started.elapsed().as_millis() as u64));
        });
    }
    drop(tx);
    let deadline = Instant::now() + wait + Duration::from_secs(10);
    let mut reports: Vec<String> = Vec::new();
    let mut pending = watchers.len();
    while pending > 0 {
        let Ok((i, outcome, ms)) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        else {
            break;
        };
        pending -= 1;
        let label = &providers[i].label;
        match outcome {
            Ok(blobs) if !blobs.is_empty() => {
                reports.push(format!("{label}=ok+{ms}ms"));
                let hexes: Vec<String> = blobs.iter().map(|b| to_hex(b)).collect();
                return format!("OK {}\t{}", hexes.join(" "), reports.join(" "));
            }
            Ok(_) => reports.push(format!("{label}=empty+{ms}ms")),
            // As in `attribution`: a quota is not a breakdown, and the log must not read as one.
            Err(error) if error.is_quota_exhausted() => {
                reports.push(format!("{label}=quota+{ms}ms:{}", short_reason(&error)))
            }
            Err(error) => reports.push(format!("{label}=fail+{ms}ms:{}", short_reason(&error))),
        }
    }
    format!("OK\t{}", reports.join(" "))
}

/// Parse a 32-byte (64-hex) per-bucket `dht_key` seed. `None` if absent/malformed — the BT-DHT leg
/// then can't address a slot and fails best-effort (N-of-many covers it); web couriers ignore it.
fn parse_dht_key(hex: &str) -> Option<[u8; 32]> {
    from_hex(hex)?.try_into().ok()
}

fn run_client(endpoint: &str, command: &str) -> i32 {
    let mut stream = match TcpStream::connect(endpoint) {
        Ok(s) => s,
        Err(error) => {
            eprintln!("connect {endpoint}: {error}");
            return 1;
        }
    };
    if writeln!(stream, "{command}").is_err() {
        eprintln!("write failed");
        return 1;
    }
    let _ = stream.flush();
    let mut line = String::new();
    match BufReader::new(stream).read_line(&mut line) {
        Ok(_) => {
            let line = line.trim_end();
            println!("{line}");
            i32::from(line.starts_with("ERR"))
        }
        Err(error) => {
            eprintln!("read failed: {error}");
            1
        }
    }
}

/// Detach this process from any inherited console so the background service shows no terminal
/// window (Windows). Raw Win32 under `#[cfg(windows)]` per the Android-portability rule (no
/// Windows crates). A no-op if there is no console.
#[cfg(windows)]
fn detach_console() {
    #[link(name = "kernel32")]
    extern "system" {
        fn FreeConsole() -> i32;
    }
    // SAFETY: FreeConsole takes no args, returns a BOOL, and is safe to call with or without a
    // console attached.
    unsafe {
        FreeConsole();
    }
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// Build the `RendezvousChannel` for a provider URL, dispatched by SCHEME: `ws(s)://` → a Nostr relay,
/// `http(s)://` → an ntfy broker, `dht://` → the BitTorrent Mainline DHT (BEP44) via the separate
/// `omni-dht-helper` process — its loopback endpoint arrives in `OMNI_DHT_HELPER_ENDPOINT` (R7 Phase
/// 2); the `dht://` host is a sentinel today (`dht://mainline`). An unrecognized scheme, or a `dht://`
/// with no helper endpoint, is skipped (`None`) rather than mis-handled — N-of-many covers a missing
/// leg. See docs/decisions/bt-dht-rendezvous-courier.md + omni-dht-helper.md.
fn channel_for_url(url: &str) -> Option<Box<dyn RendezvousChannel + Send + Sync>> {
    if url.starts_with("ws://") || url.starts_with("wss://") {
        Some(Box::new(NostrChannel::with_relay(url)))
    } else if url.starts_with("http://") || url.starts_with("https://") {
        Some(Box::new(NtfyChannel::with_base(url)))
    } else if url.starts_with("dht://") {
        // The Mainline engine runs in the separate `omni-dht-helper` process; its loopback endpoint
        // arrives via `OMNI_DHT_HELPER_ENDPOINT`. Without it we DON'T construct the courier — N-of-many
        // covers the missing DHT leg. See docs/decisions/omni-dht-helper.md.
        match std::env::var("OMNI_DHT_HELPER_ENDPOINT") {
            Ok(endpoint) if !endpoint.is_empty() => Some(Box::new(BtDhtChannel::connect(&endpoint))),
            _ => {
                eprintln!("rendezvous: no OMNI_DHT_HELPER_ENDPOINT — BT-DHT leg disabled");
                None
            }
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use omni_rendezvous::RV_BACKOFF_BASE_MS;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // The provider-list parse + default set are tested at their single source of truth
    // (`omni_rendezvous::parse_provider_urls`); the helper only maps a URL to a courier.

    #[test]
    fn url_scheme_picks_the_courier() {
        assert!(channel_for_url("https://ntfy.sh").is_some());
        assert!(channel_for_url("wss://offchain.pub").is_some());
        assert!(channel_for_url("ftp://nope").is_none());
    }

    /// A fake provider that counts its calls and either always succeeds or always errors — enough to
    /// observe whether the breaker SKIPS it. `fetch` returns one fixed blob on success.
    struct CountingChannel {
        calls: Arc<AtomicUsize>,
        ok: bool,
    }
    impl RendezvousChannel for CountingChannel {
        fn publish(&self, _t: &Topic, _k: Option<&[u8; 32]>, _b: &[u8], _ttl: u32) -> Result<(), ChannelError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.ok { Ok(()) } else { Err(ChannelError("boom".into())) }
        }
        fn fetch(&self, _t: &Topic, _k: Option<&[u8; 32]>) -> Result<Vec<Vec<u8>>, ChannelError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.ok {
                Ok(vec![vec![0xAB, 0xCD]])
            } else {
                Err(ChannelError("boom".into()))
            }
        }
    }

    /// A courier that holds at most `max` bytes and remembers what it was given.
    struct CappedChannel {
        max: usize,
        got: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    }
    impl RendezvousChannel for CappedChannel {
        fn publish(&self, _t: &Topic, _k: Option<&[u8; 32]>, b: &[u8], _ttl: u32) -> Result<(), ChannelError> {
            self.got.lock().unwrap().push(b.to_vec());
            Ok(())
        }
        fn fetch(&self, _t: &Topic, _k: Option<&[u8; 32]>) -> Result<Vec<Vec<u8>>, ChannelError> {
            Ok(Vec::new())
        }
        fn max_blob(&self) -> Option<usize> {
            Some(self.max)
        }
    }

    /// A beacon over a courier's limit goes to it as the slim copy; a courier without a limit gets the
    /// full one; a beacon within the limit goes whole everywhere.
    #[test]
    fn a_capped_courier_gets_the_slim_copy_of_a_beacon_too_big_for_it() {
        let capped = Arc::new(std::sync::Mutex::new(Vec::new()));
        let open = Arc::new(std::sync::Mutex::new(Vec::new()));
        let providers: Providers = Arc::new(vec![
            GatedProvider::new("dht".into(), Box::new(CappedChannel { max: 4, got: Arc::clone(&capped) })),
            GatedProvider::new("web".into(), Box::new(CappedChannel { max: usize::MAX, got: Arc::clone(&open) })),
        ]);
        let topic = "ab".repeat(32);
        let key = "5a".repeat(32);
        let reply = handle_line(&format!("publish {topic} {key} 0102030405 600 contact - 0a0b"), &providers);
        assert!(reply.starts_with("OK 2"), "{reply}");
        assert_eq!(capped.lock().unwrap().as_slice(), &[vec![0x0a, 0x0b]], "the slim copy");
        assert_eq!(open.lock().unwrap().as_slice(), &[vec![1, 2, 3, 4, 5]], "the full beacon");
        let reply = handle_line(&format!("publish {topic} {key} 0102 600 contact - 0a0b"), &providers);
        assert!(reply.starts_with("OK 2"), "{reply}");
        assert_eq!(capped.lock().unwrap().last().unwrap(), &vec![1, 2], "fits: whole");
    }

    /// A blob that fits a capped courier neither whole nor as a slim copy is not posted there, and the
    /// courier is not put in cooldown for it: the next beacon still reaches it.
    #[test]
    fn a_blob_too_big_for_a_capped_courier_skips_it_without_cooling_it() {
        let capped = Arc::new(std::sync::Mutex::new(Vec::new()));
        let open = Arc::new(std::sync::Mutex::new(Vec::new()));
        let providers: Providers = Arc::new(vec![
            GatedProvider::new("dht".into(), Box::new(CappedChannel { max: 4, got: Arc::clone(&capped) })),
            GatedProvider::new("web".into(), Box::new(CappedChannel { max: usize::MAX, got: Arc::clone(&open) })),
        ]);
        let topic = "ab".repeat(32);
        let key = "5a".repeat(32);
        let reply = handle_line(&format!("publish {topic} {key} 0102030405 600 background -"), &providers);
        assert!(reply.starts_with("OK 1"), "{reply}");
        assert!(capped.lock().unwrap().is_empty(), "not posted where it cannot fit");
        let reply = handle_line(&format!("publish {topic} {key} 0102 600 contact -"), &providers);
        assert!(reply.starts_with("OK 2"), "the capped courier is not cooling: {reply}");
        assert_eq!(capped.lock().unwrap().as_slice(), &[vec![1, 2]]);
    }

    /// Медленный курьер: спит фиксированное время, потом отвечает успехом.
    struct SlowChannel {
        delay: Duration,
    }
    impl RendezvousChannel for SlowChannel {
        fn publish(&self, _t: &Topic, _k: Option<&[u8; 32]>, _b: &[u8], _ttl: u32) -> Result<(), ChannelError> {
            std::thread::sleep(self.delay);
            Ok(())
        }
        fn fetch(&self, _t: &Topic, _k: Option<&[u8; 32]>) -> Result<Vec<Vec<u8>>, ChannelError> {
            std::thread::sleep(self.delay);
            Ok(Vec::new())
        }
    }

    fn slow(label: &str, delay: Duration) -> GatedProvider {
        GatedProvider::new(label.to_string(), Box::new(SlowChannel { delay }))
    }

    #[test]
    fn a_slow_courier_does_not_hold_up_the_others() {
        // Смысл N-of-many в том, что избыточность НЕ покупается задержкой. Пока веер был
        // последовательным, раунд стоил сумму: живой замер дал ntfy ~0.5 с + nostr ~1 с +
        // BT-DHT 3-10 с, и каждый добавленный релей делал рандеву медленнее.
        //
        // Порог намеренно грубый (полторы задержки при трёх курьерах): тест ловит возврат к
        // последовательному обходу, а не соревнуется с планировщиком.
        let delay = Duration::from_millis(300);
        let providers: Providers = Arc::new(vec![
            slow("test:a", delay),
            slow("test:b", delay),
            slow("test:c", delay),
        ]);

        let started = Instant::now();
        let (out, reports) = fan_out(&providers, Op::Publish, 0, |ch| ch.publish(&topic(), None, &[1], 600), |_| true);
        let elapsed = started.elapsed();

        assert_eq!(out.len(), 3, "ответить должны все три");
        assert_eq!(reports.len(), 3);
        assert!(
            elapsed < delay.mul_f32(1.5),
            "веер обошёл курьеров последовательно: {elapsed:?} при задержке {delay:?} на курьера"
        );
    }

    #[test]
    fn the_attribution_order_survives_the_concurrency() {
        // Строка атрибуции читается людьми и разбирается тестами — порядок обязан идти по набору
        // провайдеров, а не по тому, кто первым освободился.
        let fast = Arc::new(AtomicUsize::new(0));
        let providers: Providers = Arc::new(vec![
            slow("test:slow", Duration::from_millis(250)),
            labeled(&fast, true, "test:fast"),
        ]);

        let (_, reports) = fan_out(&providers, Op::Publish, 0, |ch| ch.publish(&topic(), None, &[1], 600), |_| true);
        let attr = attribution(&reports);
        let slow_at = attr.find("test:slow").expect("slow courier in the attribution");
        let fast_at = attr.find("test:fast").expect("fast courier in the attribution");
        assert!(slow_at < fast_at, "порядок сбился: {attr}");
    }

    fn gated(calls: &Arc<AtomicUsize>, ok: bool) -> GatedProvider {
        labeled(calls, ok, "test:courier")
    }

    fn labeled(calls: &Arc<AtomicUsize>, ok: bool, label: &str) -> GatedProvider {
        GatedProvider::new(label.to_string(), Box::new(CountingChannel { calls: Arc::clone(calls), ok }))
    }

    /// Records the `dht_key` arg every call saw — so we can assert the IPC handlers parse + thread it.
    struct KeyRecordingChannel {
        seen: Arc<Mutex<Vec<Option<[u8; 32]>>>>,
    }
    impl RendezvousChannel for KeyRecordingChannel {
        fn publish(&self, _t: &Topic, k: Option<&[u8; 32]>, _b: &[u8], _ttl: u32) -> Result<(), ChannelError> {
            self.seen.lock().unwrap().push(k.copied());
            Ok(())
        }
        fn fetch(&self, _t: &Topic, k: Option<&[u8; 32]>) -> Result<Vec<Vec<u8>>, ChannelError> {
            self.seen.lock().unwrap().push(k.copied());
            Ok(Vec::new())
        }
    }

    /// R7 step 3: `handle_publish`/`handle_fetch` parse the `<dhtkey_hex>` IPC arg and thread it to the
    /// channel (a valid 32-byte seed → `Some`, a malformed one → `None` so the BT-DHT leg fails
    /// best-effort while the web couriers are unaffected).
    #[test]
    fn ipc_handlers_parse_and_thread_the_dht_key() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let providers: Providers = Arc::new(vec![GatedProvider::new("test:keyrec".to_string(), Box::new(KeyRecordingChannel { seen: Arc::clone(&seen) }))]);
        let topic_hex = "ab".repeat(32);
        let dhtkey_hex = "5a".repeat(32); // 32-byte seed
        let blob_hex = "00";

        assert!(handle_publish(Some(&topic_hex), Some(&dhtkey_hex), Some(blob_hex), Some("600"), Some("contact"), Some("-"), None, None, None, &providers)
            .starts_with("OK"));
        assert!(handle_fetch(Some(&topic_hex), Some(&dhtkey_hex), Some("all"), Some("-"), &providers).starts_with("OK"));
        // Malformed key (non-hex) → None, but the call still succeeds (web couriers ignore the key).
        let _ = handle_publish(Some(&topic_hex), Some("zz"), Some(blob_hex), Some("600"), Some("contact"), Some("-"), None, None, None, &providers);

        let seen = seen.lock().unwrap();
        assert_eq!(seen[0], Some([0x5a; 32]), "publish threaded the parsed seed");
        assert_eq!(seen[1], Some([0x5a; 32]), "fetch threaded the parsed seed");
        assert_eq!(seen[2], None, "a malformed seed becomes None");
    }

    fn topic() -> Topic {
        Topic::from_hex(&"ab".repeat(32)).unwrap()
    }

    #[test]
    fn fan_out_skips_a_cooling_provider_and_keeps_using_the_healthy_one() {
        // The core fix: a rate-limited provider is parked in cooldown so the N-of-many fan-out stops
        // hammering it and works through the healthy provider instead — exactly what was needed when
        // public ntfy.sh started returning 429 mid-session.
        let bad = Arc::new(AtomicUsize::new(0));
        let good = Arc::new(AtomicUsize::new(0));
        let providers: Providers = Arc::new(vec![gated(&bad, false), gated(&good, true)]);
        let t0 = 1_000_000u64;

        let has_blob = |b: &Vec<Vec<u8>>| !b.is_empty();

        // Round 1: both tried; bad fails (→ cooldown), good returns its blob.
        let (got, _) = fan_out(&providers, Op::Read, t0, |ch| ch.fetch(&topic(), None), has_blob);
        assert_eq!(got.into_iter().flatten().count(), 1, "healthy provider returns its blob");
        assert_eq!(bad.load(Ordering::SeqCst), 1);
        assert_eq!(good.load(Ordering::SeqCst), 1);

        // Round 2 WITHIN the bad provider's cooldown: bad is SKIPPED, good still serves.
        let (got, _) =
            fan_out(&providers, Op::Read, t0 + RV_BACKOFF_BASE_MS - 1, |ch| ch.fetch(&topic(), None), has_blob);
        assert_eq!(got.into_iter().flatten().count(), 1);
        assert_eq!(bad.load(Ordering::SeqCst), 1, "rate-limited provider not hammered while cooling");
        assert_eq!(good.load(Ordering::SeqCst), 2);

        // Round 3 AFTER the cooldown expires: bad is retried (cooldown is best-effort, not a ban).
        let _ = fan_out(&providers, Op::Read, t0 + RV_BACKOFF_BASE_MS, |ch| ch.fetch(&topic(), None), has_blob);
        assert_eq!(bad.load(Ordering::SeqCst), 2, "after cooldown the provider is retried");
    }

    #[test]
    fn handlers_report_each_courier_outcome() {
        // The per-courier visibility: the publish/fetch reply carries, after a TAB, an attribution
        // naming EACH courier and its result — so the core can log which courier carried (or dropped)
        // the round instead of a bare `published=N` count.
        let good = Arc::new(AtomicUsize::new(0));
        let bad = Arc::new(AtomicUsize::new(0));
        let providers: Providers = Arc::new(vec![
            labeled(&good, true, "ntfy:ntfy.sh"),
            labeled(&bad, false, "nostr:offchain.pub"),
        ]);
        let topic_hex = "ab".repeat(32);
        let dhtkey_hex = "5a".repeat(32);

        let reply = handle_publish(Some(&topic_hex), Some(&dhtkey_hex), Some("00"), Some("600"), Some("contact"), Some("-"), None, None, None, &providers);
        let attr = reply.split('\t').nth(1).expect("publish reply carries attribution after a TAB");
        assert!(attr.contains("ntfy:ntfy.sh=ok"), "healthy courier reported ok: {attr}");
        assert!(attr.contains("nostr:offchain.pub=fail"), "failed courier reported fail: {attr}");

        // The failed courier is now cooling down → the next round SKIPS it, still reported (as skip).
        let reply = handle_fetch(Some(&topic_hex), Some(&dhtkey_hex), Some("all"), Some("-"), &providers);
        let attr = reply.split('\t').nth(1).expect("fetch reply carries attribution after a TAB");
        // good's CountingChannel returns a blob → a hit ("ok"); bad is skipped this round.
        assert!(attr.contains("ntfy:ntfy.sh=ok"), "healthy courier hit: {attr}");
        assert!(attr.contains("nostr:offchain.pub=skip"), "cooling courier skipped: {attr}");
    }

    #[test]
    fn all_cooling_down_still_probes_exactly_the_soonest() {
        // A transient hiccup that trips EVERY provider must not make rendezvous go permanently dark:
        // the next round probes exactly one (the soonest-to-recover), never zero, never all of them.
        let a = Arc::new(AtomicUsize::new(0));
        let b = Arc::new(AtomicUsize::new(0));
        let providers: Providers = Arc::new(vec![gated(&a, false), gated(&b, false)]);
        let t0 = 1_000_000u64;

        let _ = fan_out(&providers, Op::Publish, t0, |ch| ch.publish(&topic(), None, &[1], 600), |_| true);
        assert_eq!(a.load(Ordering::SeqCst), 1);
        assert_eq!(b.load(Ordering::SeqCst), 1);

        let _ = fan_out(&providers, Op::Publish, t0 + 1, |ch| ch.publish(&topic(), None, &[1], 600), |_| true);
        let probed = (a.load(Ordering::SeqCst) - 1) + (b.load(Ordering::SeqCst) - 1);
        assert_eq!(probed, 1, "all cooling → probe exactly the soonest, never fully dark, never hammer all");
    }

    /// ntfy.sh on the free tier, as measured live: publishing hits the daily message quota, reading
    /// still answers.
    struct DailyQuotaChannel {
        publishes: Arc<AtomicUsize>,
        fetches: Arc<AtomicUsize>,
    }

    impl RendezvousChannel for DailyQuotaChannel {
        fn publish(&self, _t: &Topic, _k: Option<&[u8; 32]>, _b: &[u8], _ttl: u32) -> Result<(), ChannelError> {
            self.publishes.fetch_add(1, Ordering::SeqCst);
            Err(ChannelError(
                "ntfy publish: status 429 limit reached: daily message quota reached".to_string(),
            ))
        }
        fn fetch(&self, _t: &Topic, _k: Option<&[u8; 32]>) -> Result<Vec<Vec<u8>>, ChannelError> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            Ok(vec![vec![0xAB]])
        }
    }

    /// The publishing quota closes publishing only: the courier keeps being read, while a new publish
    /// waits out the quota cooldown instead of knocking on it every round.
    #[test]
    fn a_spent_publishing_quota_does_not_stop_reading_the_courier() {
        let publishes = Arc::new(AtomicUsize::new(0));
        let fetches = Arc::new(AtomicUsize::new(0));
        let providers: Providers = Arc::new(vec![
            GatedProvider::new(
                "ntfy:ntfy.sh".to_string(),
                Box::new(DailyQuotaChannel { publishes: Arc::clone(&publishes), fetches: Arc::clone(&fetches) }),
            ),
            gated(&Arc::new(AtomicUsize::new(0)), true),
        ]);
        let t0 = 1_000_000u64;

        let (accepted, reports) = fan_out(&providers, Op::Publish, t0, |ch| ch.publish(&topic(), None, &[1], 600), |_| true);
        assert_eq!(accepted.len(), 1, "the other courier took it");
        assert!(attribution(&reports).contains("ntfy:ntfy.sh=quota"), "{}", attribution(&reports));

        let (got, _) = fan_out(&providers, Op::Read, t0 + 1, |ch| ch.fetch(&topic(), None), |b: &Vec<Vec<u8>>| !b.is_empty());
        assert_eq!(fetches.load(Ordering::SeqCst), 1, "reading the courier goes on");
        assert!(got.iter().flatten().any(|blob| blob == &vec![0xAB]));

        let _ = fan_out(&providers, Op::Publish, t0 + 2, |ch| ch.publish(&topic(), None, &[1], 600), |_| true);
        assert_eq!(publishes.load(Ordering::SeqCst), 1, "publishing waits out the quota");
        let _ = fan_out(
            &providers,
            Op::Publish,
            t0 + omni_rendezvous::RV_QUOTA_COOLDOWN_MS,
            |ch| ch.publish(&topic(), None, &[1], 600),
            |_| true,
        );
        assert_eq!(publishes.load(Ordering::SeqCst), 2, "and is tried again after it");
    }

    /// ntfy's cap on open subscriptions per IP closes subscribing only: a plain fetch goes on.
    #[test]
    fn a_refused_subscription_does_not_stop_fetching() {
        struct NoMoreSubscriptions;
        impl RendezvousChannel for NoMoreSubscriptions {
            fn publish(&self, _t: &Topic, _k: Option<&[u8; 32]>, _b: &[u8], _ttl: u32) -> Result<(), ChannelError> {
                Ok(())
            }
            fn fetch(&self, _t: &Topic, _k: Option<&[u8; 32]>) -> Result<Vec<Vec<u8>>, ChannelError> {
                Ok(vec![vec![0xCD]])
            }
            fn supports_watch(&self) -> bool {
                true
            }
            fn watch(&self, _t: &[Topic], _w: Duration) -> Result<Vec<Vec<u8>>, ChannelError> {
                Err(ChannelError("ntfy watch: status 429 limit reached: too many subscriptions".to_string()))
            }
        }
        let providers: Providers = Arc::new(vec![
            GatedProvider::new("ntfy:ntfy.sh".to_string(), Box::new(NoMoreSubscriptions)),
            labeled(&Arc::new(AtomicUsize::new(0)), true, "test:other"),
        ]);
        let reply = handle_watch(&format!(" 1 {}", topic().id()), &providers);
        assert!(reply.contains("ntfy:ntfy.sh=quota"), "{reply}");
        assert_eq!(handle_watch(&format!(" 1 {}", topic().id()), &providers), "OK	none-can-watch", "waits out its quota");
        let reply = handle_fetch(Some(&"ab".repeat(32)), Some(&"5a".repeat(32)), Some("all"), Some("-"), &providers);
        assert!(reply.contains("ntfy:ntfy.sh=ok"), "fetch goes on: {reply}");
    }

    /// A pool relay serves only its share of the topics: publishing goes to two of the pool, reading
    /// to three, the rest of the pool is neither tried nor reported, and a courier outside the pool
    /// (ntfy here) serves every topic. A relay cooling down hands its place to the next in rank.
    #[test]
    fn a_topic_goes_through_its_share_of_the_pool_and_every_courier_outside_it() {
        let calls: Vec<Arc<AtomicUsize>> = (0..7).map(|_| Arc::new(AtomicUsize::new(0))).collect();
        let mut list = vec![GatedProvider::new(
            "ntfy:ntfy.sh".to_string(),
            Box::new(CountingChannel { calls: Arc::clone(&calls[0]), ok: true }),
        )];
        for (n, url) in omni_rendezvous::NOSTR_RELAY_POOL.iter().enumerate() {
            list.push(GatedProvider::for_url(
                url,
                Box::new(CountingChannel { calls: Arc::clone(&calls[n + 1]), ok: true }),
            ));
        }
        let providers: Providers = Arc::new(list);
        let topic = topic();
        let publish = couriers_for_topic(&providers, &topic, Op::Publish, 0, None);
        let read = couriers_for_topic(&providers, &topic, Op::Read, 0, None);
        assert_eq!(publish.len(), 1 + omni_rendezvous::POOL_RELAYS_TO_PUBLISH);
        assert_eq!(read.len(), 1 + omni_rendezvous::POOL_RELAYS_TO_READ);
        assert!(publish.contains(&0) && read.contains(&0), "ntfy serves every topic");
        assert!(publish.iter().all(|i| read.contains(i)), "a reader looks where the publisher posted");

        let reply = handle_publish(Some(&topic.id()), Some(&"5a".repeat(32)), Some("00"), Some("600"), Some("contact"), Some("-"), None, None, None, &providers);
        assert!(reply.starts_with("OK 3"), "{reply}");
        assert_eq!(reply.matches("=ok").count(), 3, "only the chosen couriers are reported: {reply}");
        let touched = calls.iter().filter(|c| c.load(Ordering::SeqCst) > 0).count();
        assert_eq!(touched, 3);

        // The best-ranked pool relay cools down: the next one takes its place.
        let best = publish.iter().copied().find(|i| *i != 0).unwrap();
        providers[best].on_failure(1_000);
        let again = couriers_for_topic(&providers, &topic, Op::Publish, 1_001, None);
        assert!(!again.contains(&best) && again.len() == publish.len(), "{again:?}");
    }

    /// A fetch asks the web couriers, the DHT one, or all — the core reads the slow DHT only when the
    /// web couriers did not have it.
    #[test]
    fn a_fetch_asks_the_web_couriers_the_dht_or_all() {
        let (web, dht) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let providers: Providers = Arc::new(vec![
            GatedProvider::for_url("https://ntfy.example", Box::new(CountingChannel { calls: Arc::clone(&web), ok: true })),
            GatedProvider::for_url("dht://mainline", Box::new(CountingChannel { calls: Arc::clone(&dht), ok: true })),
        ]);
        let fetch = |legs: &str| handle_fetch(Some(&"ab".repeat(32)), Some(&"5a".repeat(32)), Some(legs), Some("-"), &providers);
        assert!(fetch("web").contains("ntfy:ntfy.example=ok") && !fetch("web").contains("dht:"));
        assert_eq!(dht.load(Ordering::SeqCst), 0, "the web fetch did not touch the DHT");
        assert!(fetch("dht").contains("dht:mainline=ok"));
        assert_eq!(dht.load(Ordering::SeqCst), 1);
        assert!(fetch("all").contains("ntfy:ntfy.example=ok"));
        assert_eq!(dht.load(Ordering::SeqCst), 2);
        assert!(fetch("bogus").starts_with("ERR"));
    }

    /// A courier low on its publishing allowance takes the beacons a contact waits for, not the
    /// background posts — those go through the other couriers.
    #[test]
    fn a_background_post_leaves_a_low_allowance_to_the_contacts() {
        struct AlmostSpent(Arc<AtomicUsize>);
        impl RendezvousChannel for AlmostSpent {
            fn publish(&self, _t: &Topic, _k: Option<&[u8; 32]>, _b: &[u8], _ttl: u32) -> Result<(), ChannelError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            fn fetch(&self, _t: &Topic, _k: Option<&[u8; 32]>) -> Result<Vec<Vec<u8>>, ChannelError> {
                Ok(Vec::new())
            }
            fn publish_allowance_low(&self) -> bool {
                true
            }
        }
        let (metered, other) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let providers: Providers = Arc::new(vec![
            GatedProvider::new("ntfy:ntfy.sh".to_string(), Box::new(AlmostSpent(Arc::clone(&metered)))),
            labeled(&other, true, "test:other"),
        ]);
        let publish = |priority: &str| {
            handle_publish(Some(&"ab".repeat(32)), Some(&"5a".repeat(32)), Some("00"), Some("600"), Some(priority), Some("-"), None, None, None, &providers)
        };
        assert!(publish("background").starts_with("OK 1"));
        assert_eq!(metered.load(Ordering::SeqCst), 0, "the metered courier was spared");
        assert!(publish("contact").starts_with("OK 2"));
        assert_eq!(metered.load(Ordering::SeqCst), 1, "a contact's beacon still goes through it");
        assert!(publish("urgent").starts_with("ERR"));
    }

    /// A courier the user set to "only read" is read and watched, never posted to — and in the pool a
    /// read-only relay hands its publishing place to the next one in rank.
    #[test]
    fn a_read_only_courier_is_read_but_never_posted_to() {
        let calls: Vec<Arc<AtomicUsize>> = (0..7).map(|_| Arc::new(AtomicUsize::new(0))).collect();
        let mut list = vec![GatedProvider::for_entry(
            "https://ntfy.example#read",
            Box::new(CountingChannel { calls: Arc::clone(&calls[0]), ok: true }),
        )];
        for (n, url) in omni_rendezvous::NOSTR_RELAY_POOL.iter().enumerate() {
            list.push(GatedProvider::for_url(url, Box::new(CountingChannel { calls: Arc::clone(&calls[n + 1]), ok: true })));
        }
        let topic = topic();
        let best = {
            let providers: Providers = Arc::new(Vec::new());
            let _ = providers;
            let mut ranked: Vec<usize> = (1..7).collect();
            ranked.sort_by_key(|i| omni_rendezvous::pool_rank(&topic, omni_rendezvous::NOSTR_RELAY_POOL[i - 1]));
            ranked[0]
        };
        list[best].read_only = true;
        let providers: Providers = Arc::new(list);
        let publish = couriers_for_topic(&providers, &topic, Op::Publish, 0, None);
        assert!(!publish.contains(&0), "a read-only courier takes no post");
        assert!(!publish.contains(&best), "a read-only pool relay neither");
        assert_eq!(publish.len(), omni_rendezvous::POOL_RELAYS_TO_PUBLISH, "the next relays take its place");
        let read = couriers_for_topic(&providers, &topic, Op::Read, 0, None);
        assert!(read.contains(&0) && read.contains(&best), "both are still read");
    }

    /// Publishing along the order: only the first kind that takes it is used; a kind out of quota is
    /// passed over to the next; asked for two kinds, two take it; a kind outside the order comes last.
    #[test]
    fn a_publish_goes_along_the_order_of_kinds_and_passes_a_spent_one_over() {
        let calls: Vec<Arc<AtomicUsize>> = (0..3).map(|_| Arc::new(AtomicUsize::new(0))).collect();
        let list = vec![
            labeled(&calls[0], true, "ntfy:ntfy.sh"),
            labeled(&calls[1], true, "nostr:nos.lol"),
            labeled(&calls[2], true, "dht:mainline"),
        ];
        let providers: Providers = Arc::new(list);
        let order: Vec<Vec<String>> = ["nostr", "dht", "ntfy"].iter().map(|k| vec![k.to_string()]).collect();
        let post = |ch: &dyn RendezvousChannel| ch.publish(&topic(), None, b"x", 60);
        let count = |i: usize| calls[i].load(Ordering::SeqCst);

        let (ok, reports) = publish_along_stages(&providers, &[0, 1, 2], &order, 1, 0, post);
        assert_eq!((ok.len(), count(0), count(1), count(2)), (1, 0, 1, 0), "nostr only");
        assert_eq!(attribution(&reports).split(' ').count(), 1, "the couriers not asked are not reported");

        // Nostr out of quota: the DHT takes it.
        providers[1].on_quota_exhausted(Op::Publish, 1);
        let (ok, _) = publish_along_stages(&providers, &[0, 1, 2], &order, 1, 2, post);
        assert_eq!((ok.len(), count(0), count(1), count(2)), (1, 0, 1, 1));

        // Two kinds asked for: the DHT and ntfy (nostr still spent).
        let (ok, _) = publish_along_stages(&providers, &[0, 1, 2], &order, 2, 3, post);
        assert_eq!((ok.len(), count(0), count(2)), (2, 1, 2));

        // A kind in no step is not posted to; two kinds in one step go at once.
        let fresh: Vec<Arc<AtomicUsize>> = (0..3).map(|_| Arc::new(AtomicUsize::new(0))).collect();
        let providers: Providers = Arc::new(vec![
            labeled(&fresh[0], true, "ntfy:ntfy.sh"),
            labeled(&fresh[1], true, "nostr:nos.lol"),
            labeled(&fresh[2], true, "dht:mainline"),
        ]);
        let together = vec![vec!["nostr".to_string(), "dht".to_string()]];
        let (ok, _) = publish_along_stages(&providers, &[0, 1, 2], &together, 1, 0, post);
        assert_eq!(
            (ok.len(), fresh[0].load(Ordering::SeqCst), fresh[1].load(Ordering::SeqCst), fresh[2].load(Ordering::SeqCst)),
            (2, 0, 1, 1),
            "nostr and Mainline at once, ntfy in no step"
        );
    }

    #[test]
    fn a_spread_token_parses() {
        assert_eq!(Spread::parse("all"), Some(Spread::All));
        assert_eq!(
            Spread::parse("2:nostr,dht,ntfy"),
            Some(Spread::Stages { reach: 2, stages: vec![vec!["nostr".into()], vec!["dht".into()], vec!["ntfy".into()]] })
        );
        assert_eq!(
            Spread::parse("1:nostr+dht,ntfy"),
            Some(Spread::Stages { reach: 1, stages: vec![vec!["nostr".into(), "dht".into()], vec!["ntfy".into()]] })
        );
        assert_eq!(Spread::parse("0:nostr"), None);
        assert_eq!(Spread::parse("1:"), None);
        assert_eq!(Spread::parse("x"), None);
    }

    /// A kind in no step is never posted to — a name beacon never reaches the BT-DHT, even when the
    /// DHT courier is up and the step's other kind takes it (find-by-name.md §3).
    #[test]
    fn a_kind_in_no_step_is_not_posted_to() {
        let calls: Vec<Arc<AtomicUsize>> = (0..2).map(|_| Arc::new(AtomicUsize::new(0))).collect();
        let providers: Providers =
            Arc::new(vec![labeled(&calls[0], true, "dht:mainline"), labeled(&calls[1], true, "ntfy:ntfy.sh")]);
        let (topic, key) = ("ab".repeat(32), "5a".repeat(32));
        let reply = handle_line(&format!("publish {topic} {key} 00 600 background - - 1:ntfy"), &providers);
        assert!(reply.starts_with("OK 1"), "{reply}");
        assert_eq!((calls[0].load(Ordering::SeqCst), calls[1].load(Ordering::SeqCst)), (0, 1));
        let everywhere = handle_line(&format!("publish {topic} {key} 00 600 background - - 1:nostr+ntfy"), &providers);
        assert!(everywhere.starts_with("OK 1"), "{everywhere}");
        assert_eq!(calls[0].load(Ordering::SeqCst), 0, "every web courier, never the DHT");
    }

    /// The command lines exactly as the core writes them (`RendezvousHelper::{publish,fetch}` in omni-app)
    /// reach their handlers whole — every argument, in order.
    #[test]
    fn a_command_line_reaches_its_handler_with_every_argument() {
        let calls = Arc::new(AtomicUsize::new(0));
        let providers: Providers = Arc::new(vec![labeled(&calls, true, "test:courier")]);
        let (topic, key) = ("ab".repeat(32), "5a".repeat(32));
        let publish = handle_line(&format!("publish {topic} {key} 00 600 contact -"), &providers);
        assert!(publish.starts_with("OK 1"), "{publish}");
        let background = handle_line(&format!("publish {topic} {key} 00 600 background -"), &providers);
        assert!(background.starts_with("OK 1"), "{background}");
        let chained = handle_line(&format!("publish {topic} {key} 00 600 contact - - all"), &providers);
        assert!(chained.starts_with("OK 1"), "{chained}");
        assert!(handle_line(&format!("publish {topic} {key} 00 600 contact - - 0:x"), &providers).starts_with("ERR bad spread"));
        let fetch = handle_line(&format!("fetch {topic} {key} web -"), &providers);
        assert!(fetch.starts_with("OK"), "{fetch}");
        assert!(handle_line(&format!("publish {topic} {key} 00 600 contact"), &providers).starts_with("ERR"), "the couriers are required");
        assert!(handle_line("nonsense", &providers).starts_with("ERR unknown"));
    }

    /// A pair's couriers: a topic goes only through the couriers the core names (pool relays chosen
    /// among those), a pair-only courier serves exactly the topics that name it, and without names a
    /// topic gets our own set and no pair-only courier.
    #[test]
    fn a_pairs_topic_goes_through_the_couriers_named_for_it() {
        let calls: Vec<Arc<AtomicUsize>> = (0..8).map(|_| Arc::new(AtomicUsize::new(0))).collect();
        let mut list = vec![GatedProvider::for_entry(
            "https://ntfy.sh",
            Box::new(CountingChannel { calls: Arc::clone(&calls[0]), ok: true }),
        )];
        for (n, url) in omni_rendezvous::NOSTR_RELAY_POOL.iter().enumerate() {
            list.push(GatedProvider::for_url(url, Box::new(CountingChannel { calls: Arc::clone(&calls[n + 1]), ok: true })));
        }
        list.push(GatedProvider::for_entry(
            "https://bob.example#pair",
            Box::new(CountingChannel { calls: Arc::clone(&calls[7]), ok: true }),
        ));
        let providers: Providers = Arc::new(list);
        let topic = topic();
        let own = couriers_for_topic(&providers, &topic, Op::Publish, 0, None);
        assert!(!own.contains(&7), "our own set has no pair-only courier");
        let names = vec!["https://bob.example".to_string(), "wss://nos.lol".to_string()];
        let pair = couriers_for_topic(&providers, &topic, Op::Publish, 0, Some(&names));
        assert_eq!(pair, vec![1, 7], "only the named couriers: {pair:?}");
        let reply = handle_line(
            &format!("publish {} {} 00 600 contact https://bob.example,wss://nos.lol", topic.id(), "5a".repeat(32)),
            &providers,
        );
        assert!(reply.starts_with("OK 2"), "{reply}");
        assert_eq!(calls[0].load(Ordering::SeqCst), 0, "ntfy was not named for this pair");
        assert_eq!(calls[7].load(Ordering::SeqCst), 1);
    }

    /// A courier with a subscription: answers its `watch` after `delay` with `blob` (none = timeout).
    struct WatchingChannel {
        delay: Duration,
        blob: Option<Vec<u8>>,
    }

    impl RendezvousChannel for WatchingChannel {
        fn publish(&self, _t: &Topic, _k: Option<&[u8; 32]>, _b: &[u8], _ttl: u32) -> Result<(), ChannelError> {
            Ok(())
        }
        fn fetch(&self, _t: &Topic, _k: Option<&[u8; 32]>) -> Result<Vec<Vec<u8>>, ChannelError> {
            Ok(Vec::new())
        }
        fn supports_watch(&self) -> bool {
            true
        }
        fn watch(&self, _t: &[Topic], _w: Duration) -> Result<Vec<Vec<u8>>, ChannelError> {
            std::thread::sleep(self.delay);
            Ok(self.blob.iter().cloned().collect())
        }
    }

    fn watching(label: &str, delay: Duration, blob: Option<Vec<u8>>) -> GatedProvider {
        GatedProvider::new(label.to_string(), Box::new(WatchingChannel { delay, blob }))
    }

    /// `watch` answers on the FIRST courier that saw a new blob — a slow sibling still waiting does
    /// not hold the answer back (a push is worth exactly its earliest copy).
    #[test]
    fn a_watch_answers_on_the_first_arrival_not_the_slowest_courier() {
        let providers: Providers = Arc::new(vec![
            watching("test:slow", Duration::from_secs(3), None),
            watching("test:fast", Duration::from_millis(50), Some(vec![0xAB])),
        ]);
        let started = Instant::now();
        let reply = handle_watch(&format!(" 5 {}", topic().id()), &providers);
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
        assert!(reply.starts_with("OK ab	"), "{reply}");
        assert!(reply.contains("test:fast=ok"), "{reply}");
    }

    /// Negative halves: nothing new until the wait ends → an empty `OK`; no courier that can watch
    /// → an empty `OK` at once (BT-DHT alone has no subscription), never an error the core retries.
    #[test]
    fn a_watch_with_nothing_new_or_no_watching_courier_is_an_empty_ok() {
        let quiet: Providers = Arc::new(vec![watching("test:quiet", Duration::from_millis(50), None)]);
        let reply = handle_watch(&format!(" 1 {}", topic().id()), &quiet);
        assert!(reply.starts_with("OK	"), "{reply}");
        assert!(reply.contains("test:quiet=empty"), "{reply}");

        let calls = Arc::new(AtomicUsize::new(0));
        let deaf: Providers = Arc::new(vec![gated(&calls, true)]);
        let started = Instant::now();
        assert_eq!(handle_watch(&format!(" 5 {}", topic().id()), &deaf), "OK	none-can-watch");
        assert!(started.elapsed() < Duration::from_millis(500));

        assert!(handle_watch(" 5 zz", &deaf).starts_with("ERR"));
        assert!(handle_watch(" x", &deaf).starts_with("ERR"));
    }
}
