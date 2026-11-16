//! One WebSocket per Nostr relay, kept while it is needed — the courier used to open a new one (TCP +
//! TLS + WebSocket) for every publish, every fetched topic and every subscription, and a relay counts
//! those against our address. See docs/decisions/rendezvous-courier-connections.md.
//!
//! The connection lives in a task on one small runtime shared by every relay of the process; the
//! courier's synchronous calls hand it work over a channel and wait for the answer. On it:
//!
//! - a fetch is a `REQ` under its own id, answered up to `EOSE`, then `CLOSE`d;
//! - a publish is an `EVENT`, answered by the relay's `OK` for that event id;
//! - a subscription ([`RelayLink::watch`]) is a `REQ` held open BETWEEN calls: what arrives while no
//!   call waits is kept for the next one, so a beacon published while the core renews its watch is not
//!   lost (the old subscription asked `since = now` anew each time and missed it).
//!
//! A connection that dies is opened again once, by the work that needs it: the fetches and publishes
//! in flight are sent again, the subscriptions asked again from a little before the drop (the events
//! already seen are not handed out twice). If that connect fails too, everything waiting gets the
//! error — the courier's breaker decides when to try again, as it always did. Timeouts are the old
//! ones, so no call waits longer than it did with a connection of its own.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use omni_rendezvous::ChannelError;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// The clocks of one relay connection.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timings {
    /// Resolve + TCP + TLS + WebSocket handshake.
    pub(crate) connect: Duration,
    /// How long a fetch waits for its `EOSE`, a publish for its `OK`.
    pub(crate) io: Duration,
    /// A fetch not answered this long is asked again within its `io`: over the same connection if the
    /// relay is answering anything else, over a new one if it has been silent since. A busy relay
    /// leaves a request on a long-lived connection unanswered now and then (offchain.pub, 11 of 35
    /// fetches in 20 min on the stand, 2026-10-27) — over a connection of its own per fetch that never
    /// showed, and an 8 s wait holds the whole fetch round, and the dial behind it.
    pub(crate) ask_again_after: Duration,
    /// A connection with nothing on it (no fetch, publish or subscription) is closed after this: a
    /// relay we stopped using is not held.
    pub(crate) idle_close: Duration,
    /// Silence from the relay this long → a ping; no answer within `pong_within` → the connection is
    /// taken for dead. Relays ping on their own too; this catches a half-open TCP connection.
    pub(crate) ping_after: Duration,
    pub(crate) pong_within: Duration,
    /// A subscription no call has waited on for this long is closed (the core renews its watches at
    /// once, so a live one is never this idle; an abandoned topic set is).
    pub(crate) sub_idle: Duration,
    /// A subscription asks for events from this long before it opened (and, reopened after a drop,
    /// before the drop): a beacon published while it was being set up is still seen.
    pub(crate) since_overlap: Duration,
}

pub(crate) const TIMINGS: Timings = Timings {
    connect: Duration::from_secs(8),
    io: Duration::from_secs(8),
    ask_again_after: Duration::from_secs(2),
    idle_close: Duration::from_secs(600),
    ping_after: Duration::from_secs(240),
    pong_within: Duration::from_secs(30),
    sub_idle: Duration::from_secs(180),
    since_overlap: Duration::from_secs(60),
};

/// Cap on events kept for one fetch or one subscription — a hostile relay can't stream us forever.
pub(crate) const MAX_EVENTS: usize = 64;
/// Fetches in flight on one connection at once; the rest wait their turn. Relays cap subscriptions per
/// connection (commonly 10–20), and the held subscriptions count too.
const MAX_QUERIES_IN_FLIGHT: usize = 6;
/// Requests arriving this soon after a failed connect get its error instead of a connect of their own.
const FAILURE_HOLD: Duration = Duration::from_secs(2);
/// The largest WebSocket message or frame taken from a relay (V-12). An event of ours is a beacon of a
/// few KiB; tungstenite's own default is 64 MiB per message.
const MAX_WS_MESSAGE: usize = 256 * 1024;

/// Where and how to connect.
pub(crate) struct LinkConfig {
    pub(crate) url: String,
    pub(crate) public_only: bool,
    pub(crate) timings: Timings,
    pub(crate) connections: Arc<AtomicU64>,
}

/// The process's runtime for every relay connection: one worker thread — the work is waiting on
/// sockets, not computing.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("omni-nostr")
            .enable_all()
            .build()
            .expect("start the Nostr courier runtime")
    })
}

enum Cmd {
    Query { filter: serde_json::Value, reply: std::sync::mpsc::Sender<Result<Vec<Vec<u8>>, ChannelError>> },
    Publish { event: serde_json::Value, id: String, reply: std::sync::mpsc::Sender<Result<(), ChannelError>> },
    /// A subscription has a caller waiting: put it on the wire if it is not there.
    Watch,
}

/// A relay's connection, as the courier's synchronous calls see it.
pub(crate) struct RelayLink {
    tx: mpsc::UnboundedSender<Cmd>,
    board: Arc<Board>,
    timings: Timings,
}

impl RelayLink {
    pub(crate) fn start(config: LinkConfig) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let board = Arc::new(Board::default());
        let timings = config.timings;
        runtime().spawn(run_link(config, rx, Arc::clone(&board)));
        Self { tx, board, timings }
    }

    /// The stored events matching `filter`, up to `EOSE`.
    pub(crate) fn query(&self, filter: serde_json::Value) -> Result<Vec<Vec<u8>>, ChannelError> {
        let (reply, answer) = std::sync::mpsc::channel();
        self.send(Cmd::Query { filter, reply })?;
        self.answer(answer)
    }

    /// Publish `event` (its id `id`) and wait for the relay's `OK`.
    pub(crate) fn publish(&self, event: serde_json::Value, id: String) -> Result<(), ChannelError> {
        let (reply, answer) = std::sync::mpsc::channel();
        self.send(Cmd::Publish { event, id, reply })?;
        self.answer(answer)
    }

    /// What arrived under `topics` (the `d` tags) since the last call, or — when nothing did — the
    /// first arrival within `wait`; empty on timeout, or when a newer call (or [`release`]) took the
    /// subscription over. The subscription stays open after the call.
    ///
    /// [`release`]: RelayLink::release
    pub(crate) fn watch(&self, topics: &[String], wait: Duration) -> Result<Vec<Vec<u8>>, ChannelError> {
        let key = sub_key(topics);
        let deadline = Instant::now() + wait;
        let generation = self.board.enter(&key, topics, self.timings.since_overlap);
        self.send(Cmd::Watch)?;
        self.board.wait(&key, generation, deadline)
    }

    /// End the waits of the calls on `topics` now, leaving what they have not taken for the next
    /// call: another courier already answered, and a wait that took something now would lose it.
    pub(crate) fn release(&self, topics: &[String]) {
        self.board.release(&sub_key(topics));
    }

    fn send(&self, cmd: Cmd) -> Result<(), ChannelError> {
        self.tx.send(cmd).map_err(|_| ChannelError("nostr: the relay connection is gone".to_string()))
    }

    /// The task answers every request within its own deadlines; this bound only guards against a
    /// task that died.
    fn answer<T>(&self, answer: std::sync::mpsc::Receiver<Result<T, ChannelError>>) -> Result<T, ChannelError> {
        let bound = self.timings.connect * 2 + self.timings.io * 2 + Duration::from_secs(5);
        answer
            .recv_timeout(bound)
            .unwrap_or_else(|_| Err(ChannelError("nostr: the relay connection did not answer".to_string())))
    }
}

fn sub_key(topics: &[String]) -> String {
    let mut sorted = topics.to_vec();
    sorted.sort();
    sorted.dedup();
    sorted.join(",")
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The subscriptions of one relay, shared between the connection task (which fills them) and the
/// callers waiting on them.
#[derive(Default)]
struct Board {
    subs: Mutex<HashMap<String, Sub>>,
    changed: Condvar,
}

struct Sub {
    topics: Vec<String>,
    /// Arrived and not taken yet.
    pending: VecDeque<Vec<u8>>,
    /// Event ids already received — a reopened subscription is asked again from before the drop.
    seen: HashSet<String>,
    /// Bumped by each new call and by `release`: a waiting call whose generation is behind leaves.
    generation: u64,
    waiting: usize,
    last_waited: Instant,
    /// Ask for events from this unix second on.
    since: u64,
    /// Why the subscription could not be held; handed to the waiting call.
    error: Option<ChannelError>,
}

impl Board {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Sub>> {
        self.subs.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    fn enter(&self, key: &str, topics: &[String], overlap: Duration) -> u64 {
        let mut subs = self.lock();
        let sub = subs.entry(key.to_string()).or_insert_with(|| Sub {
            topics: topics.to_vec(),
            pending: VecDeque::new(),
            seen: HashSet::new(),
            generation: 0,
            waiting: 0,
            last_waited: Instant::now(),
            since: now_unix().saturating_sub(overlap.as_secs()),
            error: None,
        });
        sub.generation += 1;
        sub.waiting += 1;
        sub.last_waited = Instant::now();
        sub.error = None;
        let generation = sub.generation;
        drop(subs);
        // An older call on the same subscription leaves now.
        self.changed.notify_all();
        generation
    }

    fn wait(&self, key: &str, generation: u64, deadline: Instant) -> Result<Vec<Vec<u8>>, ChannelError> {
        let mut subs = self.lock();
        loop {
            let now = Instant::now();
            let Some(sub) = subs.get_mut(key) else { return Ok(Vec::new()) };
            let outcome = if !sub.pending.is_empty() && sub.generation == generation {
                Some(Ok(sub.pending.drain(..).collect()))
            } else if sub.generation != generation {
                Some(Ok(Vec::new()))
            } else if let Some(error) = sub.error.take() {
                Some(Err(error))
            } else if now >= deadline {
                Some(Ok(Vec::new()))
            } else {
                None
            };
            if let Some(outcome) = outcome {
                sub.waiting -= 1;
                sub.last_waited = now;
                return outcome;
            }
            subs = self
                .changed
                .wait_timeout(subs, deadline.saturating_duration_since(now))
                .unwrap_or_else(|poison| poison.into_inner())
                .0;
        }
    }

    fn release(&self, key: &str) {
        if let Some(sub) = self.lock().get_mut(key) {
            sub.generation += 1;
        }
        self.changed.notify_all();
    }
}

struct Query {
    filter: serde_json::Value,
    reply: std::sync::mpsc::Sender<Result<Vec<Vec<u8>>, ChannelError>>,
    events: Vec<Vec<u8>>,
    /// When it last went on the wire; its deadline is from the first time.
    sent_at: Instant,
    deadline: Option<Instant>,
    /// Sent again once after a dropped connection.
    resent: bool,
    /// Asked again once after `ask_again_after`.
    asked_again: bool,
}

struct Publish {
    event: serde_json::Value,
    reply: std::sync::mpsc::Sender<Result<(), ChannelError>>,
    deadline: Instant,
    resent: bool,
}

/// One relay's connection and everything on it.
struct Link {
    config: LinkConfig,
    board: Arc<Board>,
    sink: Option<SplitSink<Ws, Message>>,
    stream: Option<SplitStream<Ws>>,
    /// Fetches on the wire, by subscription id, and the ones waiting for room.
    queries: HashMap<String, Query>,
    queued: VecDeque<Query>,
    /// Publishes waiting for `OK`, by event id.
    publishes: HashMap<String, Publish>,
    /// The board's subscriptions on the wire: key → subscription id.
    wire_subs: HashMap<String, String>,
    next_id: u64,
    last_inbound: Instant,
    ping_sent: Option<Instant>,
    last_busy: Instant,
    /// The last connect that failed, while it is fresh.
    last_failure: Option<(Instant, ChannelError)>,
}

async fn run_link(config: LinkConfig, mut rx: mpsc::UnboundedReceiver<Cmd>, board: Arc<Board>) {
    let mut link = Link {
        config,
        board,
        sink: None,
        stream: None,
        queries: HashMap::new(),
        queued: VecDeque::new(),
        publishes: HashMap::new(),
        wire_subs: HashMap::new(),
        next_id: 0,
        last_inbound: Instant::now(),
        ping_sent: None,
        last_busy: Instant::now(),
        last_failure: None,
    };
    loop {
        let wake = link.next_timer();
        tokio::select! {
            cmd = rx.recv() => {
                let Some(cmd) = cmd else {
                    link.close().await;
                    return;
                };
                link.on_cmd(cmd).await;
            }
            inbound = next_inbound(&mut link.stream) => match inbound {
                Some(Ok(message)) => link.on_message(message).await,
                _ => link.on_drop("the relay closed the connection").await,
            },
            _ = tokio::time::sleep_until(wake.into()) => link.on_timer().await,
        }
    }
}

async fn next_inbound(stream: &mut Option<SplitStream<Ws>>) -> Option<Result<Message, tokio_tungstenite::tungstenite::Error>> {
    match stream {
        Some(stream) => stream.next().await,
        None => std::future::pending().await,
    }
}

impl Link {
    fn connected(&self) -> bool {
        self.sink.is_some()
    }

    fn busy(&self) -> bool {
        !self.queries.is_empty() || !self.queued.is_empty() || !self.publishes.is_empty() || !self.wire_subs.is_empty()
    }

    fn fresh_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}{}", self.next_id)
    }

    async fn on_cmd(&mut self, cmd: Cmd) {
        self.last_busy = Instant::now();
        match cmd {
            Cmd::Query { filter, reply } => {
                let now = Instant::now();
                let query =
                    Query { filter, reply, events: Vec::new(), sent_at: now, deadline: None, resent: false, asked_again: false };
                self.queued.push_back(query);
            }
            Cmd::Publish { event, id, reply } => {
                let publish = Publish { event, reply, deadline: Instant::now(), resent: false };
                self.publishes.insert(id.clone(), publish);
                if self.connected() {
                    self.send_publish(&id).await;
                }
            }
            Cmd::Watch => {}
        }
        if !self.connected() {
            // The relay just could not be reached: what came in meanwhile gets that answer now rather
            // than waiting out a connect of its own (the breaker decides when to try it again).
            if let Some((at, error)) = self.last_failure.clone() {
                if at.elapsed() < FAILURE_HOLD {
                    self.fail_all(&error);
                    return;
                }
            }
            if let Err(error) = self.connect().await {
                self.last_failure = Some((Instant::now(), error.clone()));
                self.fail_all(&error);
                return;
            }
            self.last_failure = None;
            // The connect sent everything already waiting.
            return;
        }
        self.pump_queries().await;
        self.sync_subs().await;
    }

    /// Open the connection and put on it everything that waits for one.
    async fn connect(&mut self) -> Result<(), ChannelError> {
        let ws = open(&self.config).await?;
        let (sink, stream) = ws.split();
        self.sink = Some(sink);
        self.stream = Some(stream);
        self.last_inbound = Instant::now();
        self.ping_sent = None;
        self.wire_subs.clear();
        let ids: Vec<String> = self.publishes.keys().cloned().collect();
        for id in ids {
            self.send_publish(&id).await;
        }
        let requeue: Vec<Query> = self.queries.drain().map(|(_, query)| query).collect();
        for query in requeue.into_iter().rev() {
            self.queued.push_front(query);
        }
        self.pump_queries().await;
        self.sync_subs().await;
        Ok(())
    }

    async fn send(&mut self, frame: String) -> bool {
        match self.sink.as_mut() {
            Some(sink) => sink.send(Message::Text(frame.into())).await.is_ok(),
            None => false,
        }
    }

    async fn send_publish(&mut self, id: &str) {
        let Some(publish) = self.publishes.get_mut(id) else { return };
        publish.deadline = Instant::now() + self.config.timings.io;
        let frame = serde_json::json!(["EVENT", publish.event]).to_string();
        let _ = self.send(frame).await;
    }

    /// Put waiting fetches on the wire while there is room.
    async fn pump_queries(&mut self) {
        while self.connected() && self.queries.len() < MAX_QUERIES_IN_FLIGHT {
            let Some(mut query) = self.queued.pop_front() else { break };
            let id = self.fresh_id("q");
            query.sent_at = Instant::now();
            query.deadline.get_or_insert(query.sent_at + self.config.timings.io);
            let frame = serde_json::json!(["REQ", id, query.filter]).to_string();
            self.queries.insert(id, query);
            let _ = self.send(frame).await;
        }
    }

    /// Bring the wire in line with the board: open the subscriptions callers wait on, close the ones
    /// nobody has waited on for `sub_idle`.
    async fn sync_subs(&mut self) {
        let now = Instant::now();
        let sub_idle = self.config.timings.sub_idle;
        let (gone, to_open) = {
            let mut subs = self.board.lock();
            let gone: Vec<String> = subs
                .iter()
                .filter(|(_, sub)| sub.waiting == 0 && now.saturating_duration_since(sub.last_waited) >= sub_idle)
                .map(|(key, _)| key.clone())
                .collect();
            for key in &gone {
                subs.remove(key);
            }
            let to_open: Vec<(String, Vec<String>, u64)> = subs
                .iter()
                .filter(|(key, sub)| sub.waiting > 0 && !self.wire_subs.contains_key(*key))
                .map(|(key, sub)| (key.clone(), sub.topics.clone(), sub.since))
                .collect();
            (gone, to_open)
        };
        for key in gone {
            if let Some(id) = self.wire_subs.remove(&key) {
                self.close_now(id).await;
            }
        }
        if !self.connected() {
            return;
        }
        for (key, topics, since) in to_open {
            let id = self.fresh_id("w");
            let filter = serde_json::json!({ "kinds": [crate::nostr::RENDEZVOUS_KIND], "#d": topics, "since": since });
            self.wire_subs.insert(key, id.clone());
            let _ = self.send(serde_json::json!(["REQ", id, filter]).to_string()).await;
        }
    }

    async fn on_message(&mut self, message: Message) {
        self.last_inbound = Instant::now();
        self.ping_sent = None;
        let text = match message {
            Message::Text(text) => text,
            Message::Ping(payload) => {
                if let Some(sink) = self.sink.as_mut() {
                    let _ = sink.send(Message::Pong(payload)).await;
                }
                return;
            }
            Message::Close(_) => {
                self.on_drop("the relay closed the connection").await;
                return;
            }
            _ => return,
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else { return };
        let Some(parts) = value.as_array() else { return };
        let field = |n: usize| parts.get(n).and_then(|v| v.as_str()).unwrap_or_default();
        match field(0) {
            "EVENT" => self.on_event(field(1), parts.get(2)).await,
            "EOSE" => {
                if let Some(query) = self.queries.remove(field(1)) {
                    let _ = query.reply.send(Ok(query.events));
                    self.close_now(field(1).to_string()).await;
                    self.pump_queries().await;
                }
            }
            "CLOSED" => self.on_closed(field(1), field(2)).await,
            "OK" => {
                if let Some(publish) = self.publishes.remove(field(1)) {
                    let accepted = parts.get(2).and_then(|v| v.as_bool()).unwrap_or(false);
                    let reason = field(3);
                    // NIP-01's machine-readable prefixes (`rate-limited:`, `auth-required:`, …) go up
                    // as they are: the breaker tells a quota from a breakdown by them.
                    let outcome = if accepted {
                        Ok(())
                    } else if reason.is_empty() {
                        Err(ChannelError("nostr publish: relay did not accept the event".to_string()))
                    } else {
                        Err(ChannelError(format!("nostr publish: {reason}")))
                    };
                    let _ = publish.reply.send(outcome);
                }
            }
            // A relay's NOTICE is about the connection, not one request: the fetches on it end with
            // what they have, as a fetch over its own connection did.
            "NOTICE" => {
                let ids: Vec<String> = self.queries.keys().cloned().collect();
                for id in ids {
                    if let Some(query) = self.queries.remove(&id) {
                        let _ = query.reply.send(Ok(query.events));
                        self.close_now(id).await;
                    }
                }
                self.pump_queries().await;
            }
            _ => {}
        }
    }

    async fn on_event(&mut self, id: &str, event: Option<&serde_json::Value>) {
        let Some(event) = event else { return };
        let Some(blob) = event.get("content").and_then(|c| c.as_str()).and_then(crate::from_hex) else { return };
        if let Some(query) = self.queries.get_mut(id) {
            query.events.push(blob);
            if query.events.len() >= MAX_EVENTS {
                if let Some(query) = self.queries.remove(id) {
                    let _ = query.reply.send(Ok(query.events));
                }
                self.close_now(id.to_string()).await;
                self.pump_queries().await;
            }
            return;
        }
        let Some(key) = self.wire_subs.iter().find(|(_, sub_id)| sub_id.as_str() == id).map(|(key, _)| key.clone())
        else {
            return;
        };
        let event_id = event.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let created_at = event.get("created_at").and_then(|v| v.as_u64()).unwrap_or(0);
        let mut subs = self.board.lock();
        if let Some(sub) = subs.get_mut(&key) {
            if sub.seen.insert(event_id) {
                if sub.pending.len() >= MAX_EVENTS {
                    sub.pending.pop_front();
                }
                sub.pending.push_back(blob);
                sub.since = sub.since.max(created_at.min(now_unix()));
            }
            if sub.seen.len() > 4 * MAX_EVENTS {
                sub.seen.clear();
            }
        }
        drop(subs);
        self.board.changed.notify_all();
    }

    async fn on_closed(&mut self, id: &str, reason: &str) {
        if let Some(query) = self.queries.remove(id) {
            // Refused by a limit is a quota (the breaker cools the courier for reading); any other
            // reason ends the fetch with what it got, as before.
            let outcome = if ChannelError(reason.to_string()).is_quota_exhausted() {
                Err(ChannelError(format!("nostr fetch: {reason}")))
            } else {
                Ok(query.events)
            };
            let _ = query.reply.send(outcome);
            self.pump_queries().await;
            return;
        }
        let Some(key) = self.wire_subs.iter().find(|(_, sub_id)| sub_id.as_str() == id).map(|(key, _)| key.clone())
        else {
            return;
        };
        self.wire_subs.remove(&key);
        if let Some(sub) = self.board.lock().get_mut(&key) {
            sub.error = Some(ChannelError(format!("nostr watch: {reason}")));
        }
        self.board.changed.notify_all();
    }

    /// The connection is gone: open it again once for what is on it; if that fails, everything
    /// waiting gets the error.
    async fn on_drop(&mut self, why: &str) {
        self.sink = None;
        self.stream = None;
        self.wire_subs.clear();
        // The subscriptions are asked again from a little before now.
        let since = now_unix().saturating_sub(self.config.timings.since_overlap.as_secs());
        let waited_on = {
            let mut subs = self.board.lock();
            for sub in subs.values_mut() {
                sub.since = sub.since.min(since);
            }
            subs.values().any(|sub| sub.waiting > 0)
        };
        // What was already sent again once fails now; the rest goes once more.
        let error = ChannelError(format!("nostr: {why}"));
        let mut again = HashMap::new();
        for (id, mut query) in self.queries.drain() {
            if query.resent {
                let _ = query.reply.send(Err(error.clone()));
            } else {
                query.resent = true;
                again.insert(id, query);
            }
        }
        self.queries = again;
        let mut again = HashMap::new();
        for (id, mut publish) in self.publishes.drain() {
            if publish.resent {
                let _ = publish.reply.send(Err(error.clone()));
            } else {
                publish.resent = true;
                again.insert(id, publish);
            }
        }
        self.publishes = again;
        if self.queries.is_empty() && self.queued.is_empty() && self.publishes.is_empty() && !waited_on {
            return;
        }
        if let Err(error) = self.connect().await {
            self.fail_all(&error);
        }
    }

    /// Nothing could connect: every request and every waiting subscription gets `error`.
    fn fail_all(&mut self, error: &ChannelError) {
        for (_, query) in self.queries.drain() {
            let _ = query.reply.send(Err(error.clone()));
        }
        for query in self.queued.drain(..) {
            let _ = query.reply.send(Err(error.clone()));
        }
        for (_, publish) in self.publishes.drain() {
            let _ = publish.reply.send(Err(error.clone()));
        }
        let mut subs = self.board.lock();
        for sub in subs.values_mut().filter(|sub| sub.waiting > 0) {
            sub.error = Some(error.clone());
        }
        drop(subs);
        self.board.changed.notify_all();
    }

    fn next_timer(&self) -> Instant {
        let now = Instant::now();
        let timings = &self.config.timings;
        let mut next = now + timings.sub_idle;
        let mut consider = |at: Instant| next = next.min(at);
        for query in self.queries.values() {
            consider(query.deadline.unwrap_or(now));
            if !query.asked_again {
                consider(query.sent_at + timings.ask_again_after);
            }
        }
        for publish in self.publishes.values() {
            consider(publish.deadline);
        }
        if self.connected() {
            match self.ping_sent {
                Some(sent) => consider(sent + timings.pong_within),
                None => consider(self.last_inbound + timings.ping_after),
            }
            if !self.busy() {
                consider(self.last_busy + timings.idle_close);
            }
        }
        next.max(now + Duration::from_millis(10))
    }

    async fn on_timer(&mut self) {
        let now = Instant::now();
        let timings = self.config.timings;
        // A fetch past its deadline ends with what it got (a read timeout did the same). If the relay
        // sent nothing at all since it was asked, the connection is taken for dead now: every fetch on
        // it would wait out its deadline until the ping caught it minutes later (a connection of its
        // own per fetch never had that failure mode).
        let unanswered: Vec<String> = self
            .queries
            .iter()
            .filter(|(_, q)| !q.asked_again && now >= q.sent_at + timings.ask_again_after)
            .map(|(id, _)| id.clone())
            .collect();
        let mut wedged = false;
        for id in unanswered {
            let Some(mut query) = self.queries.remove(&id) else { continue };
            query.asked_again = true;
            self.close_now(id).await;
            if self.last_inbound < query.sent_at {
                // Nothing at all since it was asked: the connection, not the request.
                wedged = true;
                let parked = self.fresh_id("q");
                self.queries.insert(parked, query);
                continue;
            }
            let again = self.fresh_id("q");
            query.sent_at = now;
            let frame = serde_json::json!(["REQ", again, query.filter]).to_string();
            self.queries.insert(again, query);
            let _ = self.send(frame).await;
        }
        if wedged && self.connected() {
            self.on_drop("the relay stopped answering fetches").await;
            return;
        }
        let late: Vec<String> =
            self.queries.iter().filter(|(_, q)| q.deadline.is_some_and(|at| now >= at)).map(|(id, _)| id.clone()).collect();
        let mut silent = false;
        for id in late {
            if let Some(query) = self.queries.remove(&id) {
                silent |= self.last_inbound < query.sent_at;
                let _ = query.reply.send(Ok(query.events));
                self.close_now(id).await;
            }
        }
        if silent && self.connected() {
            self.on_drop("the relay did not answer a fetch").await;
            return;
        }
        let late: Vec<String> = self.publishes.iter().filter(|(_, p)| now >= p.deadline).map(|(id, _)| id.clone()).collect();
        for id in late {
            if let Some(publish) = self.publishes.remove(&id) {
                let _ = publish.reply.send(Err(ChannelError("nostr publish: relay did not accept the event".to_string())));
            }
        }
        self.pump_queries().await;
        self.sync_subs().await;
        if !self.connected() {
            return;
        }
        match self.ping_sent {
            Some(sent) if now >= sent + timings.pong_within => {
                self.on_drop("the relay stopped answering").await;
                return;
            }
            None if now >= self.last_inbound + timings.ping_after => {
                self.ping_sent = Some(now);
                if let Some(sink) = self.sink.as_mut() {
                    let _ = sink.send(Message::Ping(Default::default())).await;
                }
            }
            _ => {}
        }
        if !self.busy() && now >= self.last_busy + timings.idle_close {
            self.close().await;
        }
    }

    async fn close_now(&mut self, id: String) {
        let _ = self.send(serde_json::json!(["CLOSE", id]).to_string()).await;
    }

    async fn close(&mut self) {
        if let Some(mut sink) = self.sink.take() {
            let _ = sink.close().await;
        }
        self.stream = None;
        self.wire_subs.clear();
    }
}

/// Resolve, connect and shake hands, within `timings.connect`.
async fn open(config: &LinkConfig) -> Result<Ws, ChannelError> {
    let (host, port, _tls) = crate::nostr::parse_ws_url(&config.url)?;
    let public_only = config.public_only;
    let connect = async {
        let target = format!("{host}:{port}");
        let resolved = tokio::task::spawn_blocking(move || {
            if public_only {
                crate::resolve_public(&target).map(|addrs| addrs[0])
            } else {
                std::net::ToSocketAddrs::to_socket_addrs(&target)
                    .and_then(|mut addrs| addrs.next().ok_or_else(|| std::io::Error::other("no address")))
            }
        })
        .await
        .map_err(|e| ChannelError(format!("nostr resolve {host}: {e}")))?
        .map_err(|e| ChannelError(format!("nostr resolve {host}: {e}")))?;
        config.connections.fetch_add(1, Ordering::Relaxed);
        let stream = tokio::net::TcpStream::connect(resolved)
            .await
            .map_err(|e| ChannelError(format!("nostr connect {resolved}: {e}")))?;
        let _ = stream.set_nodelay(true);
        let ws_config = WebSocketConfig::default().max_message_size(Some(MAX_WS_MESSAGE)).max_frame_size(Some(MAX_WS_MESSAGE));
        let (ws, _response) = tokio_tungstenite::client_async_tls_with_config(config.url.as_str(), stream, Some(ws_config), None)
            .await
            .map_err(|e| ChannelError(format!("nostr ws handshake: {e}")))?;
        Ok(ws)
    };
    tokio::time::timeout(config.timings.connect, connect)
        .await
        .unwrap_or_else(|_| Err(ChannelError(format!("nostr connect {}: timed out", config.url))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nostr::NostrChannel;
    use omni_rendezvous::{RendezvousChannel, Topic};
    use std::sync::atomic::AtomicBool;

    /// A NIP-01 relay in the test process: stores events, answers `REQ` with the stored matches and
    /// `EOSE`, pushes later ones to the open subscriptions, acks `EVENT` with `OK`. It can drop every
    /// connection at once (`kill_connections`) and refuse subscriptions (`refuse_subs`).
    struct FakeRelay {
        url: String,
        accepted: Arc<AtomicU64>,
        kill: tokio::sync::watch::Sender<u64>,
        refuse_subs: Arc<AtomicBool>,
        mute_reqs: Arc<AtomicBool>,
        /// The next this many `REQ`s are left unanswered, as a busy relay does now and then.
        lose_reqs: Arc<AtomicU64>,
        _runtime: tokio::runtime::Runtime,
    }

    impl FakeRelay {
        fn start() -> Self {
            let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
            let listener = runtime.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
            let url = format!("ws://{}", listener.local_addr().unwrap());
            let accepted = Arc::new(AtomicU64::new(0));
            let (kill, _) = tokio::sync::watch::channel(0u64);
            let refuse_subs = Arc::new(AtomicBool::new(false));
            let mute_reqs = Arc::new(AtomicBool::new(false));
            let lose_reqs = Arc::new(AtomicU64::new(0));
            let (events_tx, _) = tokio::sync::broadcast::channel::<serde_json::Value>(256);
            let stored = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
            let (accepted_c, kill_c, refuse_c, mute_c, lose_c) =
                (Arc::clone(&accepted), kill.clone(), Arc::clone(&refuse_subs), Arc::clone(&mute_reqs), Arc::clone(&lose_reqs));
            runtime.spawn(async move {
                loop {
                    let Ok((socket, _)) = listener.accept().await else { return };
                    accepted_c.fetch_add(1, Ordering::SeqCst);
                    let (stored, events_tx, refuse, mute, lose) =
                        (Arc::clone(&stored), events_tx.clone(), Arc::clone(&refuse_c), Arc::clone(&mute_c), Arc::clone(&lose_c));
                    let mut kill = kill_c.subscribe();
                    tokio::spawn(async move {
                        let Ok(ws) = tokio_tungstenite::accept_async(socket).await else { return };
                        let (mut sink, mut stream) = ws.split();
                        let mut live = events_tx.subscribe();
                        let mut subs: HashMap<String, serde_json::Value> = HashMap::new();
                        loop {
                            tokio::select! {
                                _ = kill.changed() => return,
                                event = live.recv() => {
                                    let Ok(event) = event else { continue };
                                    for (id, filter) in &subs {
                                        if matches(filter, &event) {
                                            let frame = serde_json::json!(["EVENT", id, event]).to_string();
                                            let _ = sink.send(Message::Text(frame.into())).await;
                                        }
                                    }
                                }
                                inbound = stream.next() => {
                                    let Some(Ok(Message::Text(text))) = inbound else { return };
                                    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                                    match value[0].as_str().unwrap() {
                                        "EVENT" => {
                                            let event = value[1].clone();
                                            stored.lock().unwrap().push(event.clone());
                                            let frame = serde_json::json!(["OK", event["id"], true, ""]).to_string();
                                            let _ = sink.send(Message::Text(frame.into())).await;
                                            let _ = events_tx.send(event);
                                        }
                                        "REQ" => {
                                            let id = value[1].as_str().unwrap().to_string();
                                            if mute.load(Ordering::SeqCst) {
                                                continue;
                                            }
                                            if lose.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok() {
                                                continue;
                                            }
                                            if refuse.load(Ordering::SeqCst) {
                                                let frame = serde_json::json!(["CLOSED", id, "rate-limited: slow down"]).to_string();
                                                let _ = sink.send(Message::Text(frame.into())).await;
                                                continue;
                                            }
                                            let filter = value[2].clone();
                                            let hits: Vec<serde_json::Value> =
                                                stored.lock().unwrap().iter().filter(|e| matches(&filter, e)).cloned().collect();
                                            for event in hits {
                                                let frame = serde_json::json!(["EVENT", id, event]).to_string();
                                                let _ = sink.send(Message::Text(frame.into())).await;
                                            }
                                            let frame = serde_json::json!(["EOSE", id]).to_string();
                                            let _ = sink.send(Message::Text(frame.into())).await;
                                            subs.insert(id, filter);
                                        }
                                        "CLOSE" => {
                                            subs.remove(value[1].as_str().unwrap());
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    });
                }
            });
            Self { url, accepted, kill, refuse_subs, mute_reqs, lose_reqs, _runtime: runtime }
        }

        fn kill_connections(&self) {
            self.kill.send_modify(|n| *n += 1);
        }
    }

    fn matches(filter: &serde_json::Value, event: &serde_json::Value) -> bool {
        let d = event["tags"][0][1].as_str().unwrap_or_default();
        let wanted = filter["#d"].as_array().is_some_and(|ds| ds.iter().any(|x| x.as_str() == Some(d)));
        let since = filter["since"].as_u64().unwrap_or(0);
        wanted && event["created_at"].as_u64().unwrap_or(0) >= since
    }

    fn topic(n: u8) -> Topic {
        Topic::from_hex(&format!("{n:02x}").repeat(32)).unwrap()
    }

    #[test]
    fn one_connection_carries_fetches_publishes_and_a_subscription() {
        let relay = FakeRelay::start();
        let channel = NostrChannel::with_relay(relay.url.clone());
        for n in 1..=3 {
            channel.publish(&topic(n), None, &[n], 600).unwrap();
        }
        assert_eq!(channel.fetch(&topic(1), None).unwrap(), vec![vec![1]]);
        assert_eq!(channel.fetch_many(&[topic(2), topic(3)]).unwrap(), vec![vec![2], vec![3]]);
        // A subscription on the same connection sees a beacon another writer publishes.
        let other = NostrChannel::with_relay(relay.url.clone());
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| channel.watch(&[topic(9)], Duration::from_secs(10)));
            std::thread::sleep(Duration::from_millis(300));
            other.publish(&topic(9), None, &[9], 600).unwrap();
            assert_eq!(waiting.join().unwrap().unwrap(), vec![vec![9]]);
        });
        assert_eq!(channel.connections_opened(), Some(1), "everything over one connection");
    }

    #[test]
    fn a_beacon_published_between_two_watch_calls_is_handed_to_the_next() {
        let relay = FakeRelay::start();
        let channel = NostrChannel::with_relay(relay.url.clone());
        let other = NostrChannel::with_relay(relay.url.clone());
        assert!(channel.watch(&[topic(4)], Duration::from_millis(300)).unwrap().is_empty(), "nothing yet");
        // No call waits now: the old subscription was closed here, and this beacon was lost to it.
        other.publish(&topic(4), None, &[4], 600).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        assert_eq!(channel.watch(&[topic(4)], Duration::from_secs(10)).unwrap(), vec![vec![4]]);
        assert!(started.elapsed() < Duration::from_secs(1), "handed over at once, not waited for");
        assert!(channel.watch(&[topic(4)], Duration::from_millis(300)).unwrap().is_empty(), "and only once");
    }

    #[test]
    fn a_dropped_connection_is_opened_again_and_the_subscription_misses_nothing() {
        let relay = FakeRelay::start();
        let channel = NostrChannel::with_relay(relay.url.clone());
        let other = NostrChannel::with_relay(relay.url.clone());
        assert!(channel.watch(&[topic(5)], Duration::from_millis(300)).unwrap().is_empty());
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| channel.watch(&[topic(5)], Duration::from_secs(10)));
            std::thread::sleep(Duration::from_millis(300));
            relay.kill_connections();
            // Published while the subscriber's connection is down or being opened again.
            other.publish(&topic(5), None, &[5], 600).unwrap();
            assert_eq!(waiting.join().unwrap().unwrap(), vec![vec![5]], "seen after the reconnect");
        });
        assert_eq!(channel.connections_opened(), Some(2));
        assert_eq!(channel.fetch(&topic(5), None).unwrap(), vec![vec![5]], "and the connection works");
    }

    #[test]
    fn release_ends_a_wait_and_leaves_what_comes_next_for_the_next_call() {
        let relay = FakeRelay::start();
        let channel = NostrChannel::with_relay(relay.url.clone());
        let other = NostrChannel::with_relay(relay.url.clone());
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| {
                let started = Instant::now();
                (channel.watch(&[topic(6)], Duration::from_secs(10)).unwrap(), started.elapsed())
            });
            std::thread::sleep(Duration::from_millis(300));
            channel.release_watch(&[topic(6)]);
            let (got, waited) = waiting.join().unwrap();
            assert!(got.is_empty() && waited < Duration::from_secs(2), "released at once, nothing taken");
        });
        other.publish(&topic(6), None, &[6], 600).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(channel.watch(&[topic(6)], Duration::from_secs(5)).unwrap(), vec![vec![6]]);
    }

    #[test]
    fn an_idle_connection_is_closed_and_opened_again_when_needed() {
        let relay = FakeRelay::start();
        let connections = Arc::new(AtomicU64::new(0));
        let timings = Timings { idle_close: Duration::from_millis(300), ..TIMINGS };
        let link = RelayLink::start(LinkConfig {
            url: relay.url.clone(),
            public_only: false,
            timings,
            connections: Arc::clone(&connections),
        });
        let filter = serde_json::json!({ "kinds": [crate::nostr::RENDEZVOUS_KIND], "#d": ["x"] });
        assert!(link.query(filter.clone()).unwrap().is_empty());
        assert!(link.query(filter.clone()).unwrap().is_empty());
        assert_eq!(connections.load(Ordering::SeqCst), 1, "back to back: one connection");
        std::thread::sleep(Duration::from_millis(900));
        assert!(link.query(filter).unwrap().is_empty());
        assert_eq!(connections.load(Ordering::SeqCst), 2, "closed when idle, opened again on demand");
        assert_eq!(relay.accepted.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_connection_that_leaves_a_fetch_unanswered_is_opened_anew_for_the_next() {
        let relay = FakeRelay::start();
        let connections = Arc::new(AtomicU64::new(0));
        let link = RelayLink::start(LinkConfig {
            url: relay.url.clone(),
            public_only: false,
            timings: Timings { io: Duration::from_millis(300), ..TIMINGS },
            connections: Arc::clone(&connections),
        });
        let filter = serde_json::json!({ "kinds": [crate::nostr::RENDEZVOUS_KIND], "#d": ["x"] });
        assert!(link.query(filter.clone()).unwrap().is_empty());
        relay.mute_reqs.store(true, Ordering::SeqCst);
        let started = Instant::now();
        assert!(link.query(filter.clone()).unwrap().is_empty(), "times out with nothing, as before");
        assert!(started.elapsed() < Duration::from_secs(2));
        relay.mute_reqs.store(false, Ordering::SeqCst);
        assert!(link.query(filter).unwrap().is_empty());
        assert_eq!(connections.load(Ordering::SeqCst), 2, "the silent connection was not reused");
    }

    fn quick_asking_link(relay: &FakeRelay, connections: &Arc<AtomicU64>) -> RelayLink {
        RelayLink::start(LinkConfig {
            url: relay.url.clone(),
            public_only: false,
            timings: Timings { ask_again_after: Duration::from_millis(300), ..TIMINGS },
            connections: Arc::clone(connections),
        })
    }

    fn d_filter(topic: &Topic) -> serde_json::Value {
        serde_json::json!({ "kinds": [crate::nostr::RENDEZVOUS_KIND], "#d": [topic.id()] })
    }

    #[test]
    fn a_fetch_the_relay_left_unanswered_is_asked_again_over_the_same_connection() {
        let relay = FakeRelay::start();
        let writer = NostrChannel::with_relay(relay.url.clone());
        writer.publish(&topic(0x21), None, &[0x21], 600).unwrap();
        writer.publish(&topic(0x22), None, &[0x22], 600).unwrap();
        let connections = Arc::new(AtomicU64::new(0));
        let link = quick_asking_link(&relay, &connections);
        assert!(link.query(d_filter(&topic(0x20))).unwrap().is_empty(), "connected");
        relay.lose_reqs.store(1, Ordering::SeqCst);
        let started = Instant::now();
        let (first, second) = std::thread::scope(|scope| {
            let first = scope.spawn(|| link.query(d_filter(&topic(0x21))));
            let second = scope.spawn(|| link.query(d_filter(&topic(0x22))));
            (first.join().unwrap().unwrap(), second.join().unwrap().unwrap())
        });
        assert_eq!((first, second), (vec![vec![0x21]], vec![vec![0x22]]), "the lost one was asked again");
        assert!(started.elapsed() < Duration::from_secs(2), "not the 8 s deadline: {:?}", started.elapsed());
        assert_eq!(connections.load(Ordering::SeqCst), 1, "the relay was answering: same connection");
    }

    #[test]
    fn a_connection_silent_since_a_fetch_was_asked_is_replaced_within_the_fetch() {
        let relay = FakeRelay::start();
        let writer = NostrChannel::with_relay(relay.url.clone());
        writer.publish(&topic(0x23), None, &[0x23], 600).unwrap();
        let connections = Arc::new(AtomicU64::new(0));
        let link = quick_asking_link(&relay, &connections);
        assert!(link.query(d_filter(&topic(0x20))).unwrap().is_empty(), "connected");
        relay.lose_reqs.store(1, Ordering::SeqCst);
        let started = Instant::now();
        assert_eq!(link.query(d_filter(&topic(0x23))).unwrap(), vec![vec![0x23]]);
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        assert_eq!(connections.load(Ordering::SeqCst), 2, "asked again over a new connection");
    }

    #[test]
    fn requests_right_after_a_failed_connect_get_its_error_without_a_connect_of_their_own() {
        // A port nothing listens on: every connect is refused.
        let url = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("ws://{}", listener.local_addr().unwrap())
        };
        let connections = Arc::new(AtomicU64::new(0));
        let link = RelayLink::start(LinkConfig { url, public_only: false, timings: TIMINGS, connections: Arc::clone(&connections) });
        let filter = serde_json::json!({ "kinds": [crate::nostr::RENDEZVOUS_KIND], "#d": ["x"] });
        assert!(link.query(filter.clone()).is_err());
        assert!(link.query(filter.clone()).is_err());
        assert_eq!(connections.load(Ordering::SeqCst), 1, "the second got the first one's answer");
        std::thread::sleep(FAILURE_HOLD + Duration::from_millis(100));
        assert!(link.query(filter).is_err());
        assert_eq!(connections.load(Ordering::SeqCst), 2, "and later the relay is tried again");
    }

    /// How real relays treat many requests on one connection: a held subscription, then fetches one
    /// after another and two at a time. Prints each fetch's time; a stall shows as ~8 s.
    #[test]
    #[ignore = "network: hits real public Nostr relays"]
    fn live_relays_answer_every_fetch_on_one_connection() {
        for url in ["wss://offchain.pub", "wss://nos.lol", "wss://relay.nostr.net"] {
            let channel = NostrChannel::with_relay(url);
            let held = std::thread::scope(|scope| {
                let watch = scope.spawn(|| channel.watch(&[topic(0x31)], Duration::from_secs(20)));
                std::thread::sleep(Duration::from_millis(500));
                let mut times = Vec::new();
                for n in 0..6u8 {
                    let started = Instant::now();
                    let got = channel.fetch(&topic(0x40 + n), None);
                    times.push(format!("{}{}", started.elapsed().as_millis(), if got.is_err() { "E" } else { "" }));
                }
                for n in 0..4u8 {
                    let started = Instant::now();
                    // The second pair: the topic the subscription holds, as a dial reads it.
                    let (ta, tb) = if n < 2 { (topic(0x50 + n), topic(0x60 + n)) } else { (topic(0x31), topic(0x60 + n)) };
                    let (a, b) = std::thread::scope(|inner| {
                        let a = inner.spawn(|| channel.fetch(&ta, None));
                        let b = inner.spawn(|| channel.fetch(&tb, None));
                        (a.join().unwrap(), b.join().unwrap())
                    });
                    times.push(format!("p{}{}", started.elapsed().as_millis(), if a.is_err() || b.is_err() { "E" } else { "" }));
                }
                channel.release_watch(&[topic(0x31)]);
                let _ = watch.join();
                times
            });
            println!("{url}: conn={:?} {}", channel.connections_opened(), held.join(" "));
        }
    }

    #[test]
    fn a_subscription_the_relay_refuses_by_a_limit_comes_back_as_a_quota() {
        let relay = FakeRelay::start();
        relay.refuse_subs.store(true, Ordering::SeqCst);
        let channel = NostrChannel::with_relay(relay.url.clone());
        let error = channel.watch(&[topic(7)], Duration::from_secs(5)).unwrap_err();
        assert!(error.is_quota_exhausted(), "{error:?}");
        let error = channel.fetch(&topic(7), None).unwrap_err();
        assert!(error.is_quota_exhausted(), "{error:?}");
    }
}
