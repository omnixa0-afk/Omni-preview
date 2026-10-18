// QUIC over UDP — primary point-to-point Interface implementation.
// См. docs/architecture/networking-overlay.md §«Interface abstraction».
//
// Phase α-1 (2026-05-26): updated с production-grade additions:
// - Identity-derived TLS cert (via cert::derive_cert_from_signing_key) —
//   deterministic cert per identity, no random rcgen
// - **Outbound dials также spawn'ят recv loop** (раньше только incoming
//   имели recv loop — fix F2 из spike-results)
// - Custom verifier на client side остаётся AcceptAny (по design'у —
//   identity verification отдельно через challenge-response, см. cert.rs)
//
// Identity verification (challenge-response after handshake) — это
// Phase α-2 deliverable. Сейчас QuicOverUdpInterface обеспечивает только
// encrypted channel; trust gating живёт layer выше.

use crate::cert::derive_cert_from_signing_key;
use crate::interface::{BulkStream, Interface, InterfaceId, InterfaceKind, NeighborId};
use crate::mux_socket::MuxRouter;
use crate::stun_socket::StunSocket;
use crate::tcp_transport::{tcp_trace, trace_enabled};
use async_trait::async_trait;
use bytes::Bytes;
use ed25519_dalek::SigningKey;
use omni_net_primitives::bdp_cap::BdpCapSnapshot;
use omni_net_primitives::{bind_quic_udp_socket, omni_client_config_for, omni_server_config_for, TransportCarrier};
use parking_lot::Mutex as SyncMutex;
use quinn::{Endpoint, EndpointConfig, Runtime, TokioRuntime};
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex as AsyncMutex};

/// Marks a QUIC bi-stream as belonging to the BULK plane (attachment transfer). Written by the
/// opener before any payload and verified by the acceptor, so a bi-stream opened for anything else
/// is rejected rather than mis-read as transfer data.
const BULK_STREAM_PREFACE: [u8; 4] = *b"OMNB";

/// ALPN for mesh QUIC connections: `h3`, the standard HTTP/3 ALPN, so the QUIC ClientHello (readable on
/// path — QUIC v1 protects it only with the DCID-derived Initial secret) carries no Omni-specific
/// identifier. This is **all-or-nothing**: the client offers ONLY this ALPN (offering
/// both would re-expose the old value in the list), so every node must run the same value or the TLS
/// handshake fails on no-common-ALPN. Pre-v1 flag-day (CONTRIBUTING.md: flip together, no legacy) — a node
/// on this binary cannot reach a peer/relay still on the old ALPN until it is rebuilt.
const OMNI_MESH_ALPN: &[u8] = b"h3";

/// The SNI on every outgoing QUIC/TLS dial. Not a real hostname — the far end uses an accept-any TLS
/// verifier and ignores it; identity is the in-tunnel ed25519 challenge.
const OMNI_SNI: &str = "localhost";

// TLS client/server config + cert verifier теперь из omni-net-primitives
// (omni_client_config / omni_server_config / OmniServerCertVerifier).
// Identity verification — отдельный layer через challenge-response
// (cert.rs), выполняется after QUIC handshake'а. Pattern proven в
// iroh / libp2p. См. docs/bugs/mesh-vs-existing-duplication-audit.md.

/// Point-to-point QUIC interface. Binds UDP socket, accepts incoming
/// connections, can dial outgoing.
pub struct QuicOverUdpInterface {
    id: InterfaceId,
    /// Carrier kind reported by `kind()`: `QuicOverUdp` for the native UDP socket, `QuicOverTcp` when
    /// the endpoint rides a `TcpQuicSocket` (relay TLS-over-TCP). Drives connection-quality path
    /// ranking (`path_rank`). See docs/decisions/connection-quality-ladder.md.
    kind: InterfaceKind,
    endpoint: Endpoint,
    // tokio::Mutex потому что recv_frame await'ит держа guard'а; std/parking_lot
    // mutex'ом не пропустим Send-границу.
    inbound_rx: AsyncMutex<mpsc::Receiver<(NeighborId, Vec<u8>)>>,
    // parking_lot для соединений — короткие критические секции, без await'а внутри.
    // Ключ — `connection_neighbor_id`: одно СОЕДИНЕНИЕ, а не адрес собеседника. `Arc` — чтобы
    // соединение убирало себя отсюда, когда закроется (см. `adopt_connection`).
    connections: Arc<SyncMutex<HashMap<NeighborId, quinn::Connection>>>,
    // Sender для inbound recv loop'ов — даём outbound dial path'у возможность
    // spawn'ить recv loop для своих connection'ов (fix F2).
    inbound_tx: mpsc::Sender<(NeighborId, Vec<u8>)>,
    // Unreliable datagram plane (realtime media). Separate channel from the
    // stream/frame plane: datagrams carry raw bytes, no NetworkFrame header.
    datagram_rx: AsyncMutex<mpsc::Receiver<(NeighborId, Vec<u8>)>>,
    datagram_tx: mpsc::Sender<(NeighborId, Vec<u8>)>,
    // Connection-close plane: a neighbour's id is emitted here when its QUIC
    // connection ends (recv_loop's accept_uni errors). The mesh prunes that path
    // proactively. P2 PeerLink cleanup-on-close.
    closed_rx: AsyncMutex<mpsc::Receiver<NeighborId>>,
    closed_tx: mpsc::Sender<NeighborId>,
    // Bulk plane: long-lived ordered byte pipes (attachment transfer), one QUIC bi-stream each,
    // opened on the peer's EXISTING connection. Kept apart from the frame plane so a multi-gigabyte
    // transfer can never head-of-line-block chat/control frames.
    bulk_rx: AsyncMutex<mpsc::Receiver<(NeighborId, BulkStream)>>,
    bulk_tx: mpsc::Sender<(NeighborId, BulkStream)>,
    /// Whether our socket is IPv6 (incl. dual-stack `[::]`). A dual-stack socket must
    /// address IPv4 dial targets as v4-mapped (`::ffff:a.b.c.d`) or quinn can't route them —
    /// so binding v6 to gain native-v6 reach never regresses dialing v4 peers. `false` for
    /// the v4-only (proven) bind, where the mapping is a no-op.
    local_is_ipv6: bool,
    /// The per-destination carrier router under the UDP endpoint (`Some` for the native-UDP carrier,
    /// `None` for a pure TCP carrier). Lets a peer's ONE quinn connection ride the UDP base or its
    /// direct-TCP door, switched by [`route_peer_over_door`](Self::route_peer_over_door) — the switch
    /// is a QUIC path migration, not a new connection. See [`crate::mux_socket`].
    mux: Option<Arc<MuxRouter>>,
    /// STUN on THIS endpoint's socket (Some for the native-UDP carrier): the reflexive address it
    /// learns is the mapping a peer's dial actually reaches. See [`crate::stun_socket`].
    stun: Option<Arc<StunSocket>>,
    /// Клиентский конфиг, которым дозваниваемся. Живёт ОТДЕЛЬНО от endpoint'а, потому что
    /// `set_default_client_config` требует `&mut`, а интерфейс живёт в `Arc`. Дозвон идёт через
    /// `connect_with`, так что смену контроллера перегрузки видит следующий же дозвон.
    client_config: SyncMutex<quinn::ClientConfig>,
    /// The same two for a connection over TCP — a dial the mux routes into a peer's door, an accept
    /// from our own door's bridge: it runs with the door's queue budget
    /// (`omni_net_primitives::DOOR_QUEUE_BUDGET`). On a TCP carrier these and the two above are one.
    door_client_config: SyncMutex<quinn::ClientConfig>,
    door_server_config: SyncMutex<Arc<quinn::ServerConfig>>,
    /// Пересобрать конфиги из ТЕКУЩЕГО решения о перегрузке. Замыкание, а не пара
    /// cert/key полями: ключи так не лежат в структуре и не переживают её отладочный вывод.
    rebuild_configs: Box<dyn Fn() -> io::Result<CarrierConfigs> + Send + Sync>,
}

/// The endpoint's configs: for its own carrier, and for a connection that rides TCP under it.
struct CarrierConfigs {
    server: quinn::ServerConfig,
    client: quinn::ClientConfig,
    door_server: quinn::ServerConfig,
    door_client: quinn::ClientConfig,
}

impl std::fmt::Debug for QuicOverUdpInterface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuicOverUdpInterface")
            .field("id", &self.id)
            .field("local_addr", &self.endpoint.local_addr())
            .finish_non_exhaustive()
    }
}

/// Which carrier a [`QuicOverUdpInterface`] rides — chosen explicitly by the caller so a node can
/// stand up one of each for dual-carrier coexistence. See docs/decisions/connection-quality-ladder.md.
enum CarrierChoice {
    /// Native UDP socket bound at this address (rungs 1–2: direct + UDP-to-relay).
    Udp(SocketAddr),
    /// TLS-over-TCP `TcpQuicSocket` to the relay bridge at this address (rung 0). A peer's OWN direct-
    /// TCP front door is NOT a separate carrier any more — Meta-B routes a peer's ONE UDP-carrier quinn
    /// connection over its door via QUIC migration (see [`crate::mux_socket`]), so only the relay uses
    /// this pure-TCP carrier.
    TcpRelay(SocketAddr),
}

impl QuicOverUdpInterface {
    /// Compat entry: honours the legacy `OMNI_RELAY_TCP` env (set ⇒ TLS-over-TCP relay carrier),
    /// else binds the native UDP carrier. Prefer `bind_udp` / `bind_tcp_relay` for dual-carrier
    /// coexistence. `bind_addr` = "0.0.0.0:0" random / "127.0.0.1:8443" fixed.
    pub fn bind(bind_addr: SocketAddr, signing_key: &SigningKey) -> io::Result<Arc<Self>> {
        let choice = match std::env::var("OMNI_RELAY_TCP") {
            Ok(relay_tcp) if !relay_tcp.is_empty() => CarrierChoice::TcpRelay(
                relay_tcp.parse().map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("OMNI_RELAY_TCP '{relay_tcp}': {e}"),
                    )
                })?,
            ),
            _ => CarrierChoice::Udp(bind_addr),
        };
        Self::bind_carrier(choice, signing_key)
    }

    /// Bind the native UDP carrier (rungs 1–2: direct + UDP-to-relay).
    pub fn bind_udp(bind_addr: SocketAddr, signing_key: &SigningKey) -> io::Result<Arc<Self>> {
        Self::bind_carrier(CarrierChoice::Udp(bind_addr), signing_key)
    }

    /// Stand up the TLS-over-TCP relay carrier (rung 0) to `relay_addr` (the relay bridge, e.g. :8443).
    pub fn bind_tcp_relay(relay_addr: SocketAddr, signing_key: &SigningKey) -> io::Result<Arc<Self>> {
        Self::bind_carrier(CarrierChoice::TcpRelay(relay_addr), signing_key)
    }

    /// Core constructor: build the QUIC endpoint over the chosen carrier (see the wrappers above).
    /// `signing_key` derives the TLS cert.
    fn bind_carrier(choice: CarrierChoice, signing_key: &SigningKey) -> io::Result<Arc<Self>> {
        let _ = rustls::crypto::ring::default_provider().install_default();

        // Cert deriv'нут из identity. Config builders + cert verifier — из
        // omni-net-primitives (shared с omni-transport). ALPN `h3` (см. OMNI_MESH_ALPN) — стандартный
        // ALPN HTTP/3; идентичность — in-tunnel ed25519, не ALPN.
        // Конфиги строятся ЗАМЫКАНИЕМ, а не один раз: контроллер перегрузки выбирается внутри
        // `omni_transport_config`, и пересборка — единственный способ применить смену настройки,
        // не перезапуская меш (quinn не умеет менять контроллер на живом соединении).
        let signing_key_for_rebuild = signing_key.clone();
        // A TCP carrier's every connection rides TCP.
        let own_carrier = match &choice {
            CarrierChoice::TcpRelay(_) => TransportCarrier::Tcp,
            CarrierChoice::Udp(_) => TransportCarrier::Udp,
        };
        let rebuild_configs = move || -> io::Result<CarrierConfigs> {
            let (cert, key) = derive_cert_from_signing_key(&signing_key_for_rebuild)
                .map_err(|e| io::Error::other(format!("cert derive: {e}")))?;
            let server_for = |carrier| {
                omni_server_config_for(cert.clone(), key.clone_key(), OMNI_MESH_ALPN, carrier)
                    .map_err(|e| io::Error::other(format!("server config: {e}")))
            };
            let client_for = |carrier| {
                omni_client_config_for(OMNI_MESH_ALPN, carrier)
                    .map_err(|e| io::Error::other(format!("client config: {e}")))
            };
            Ok(CarrierConfigs {
                server: server_for(own_carrier)?,
                client: client_for(own_carrier)?,
                door_server: server_for(TransportCarrier::Tcp)?,
                door_client: client_for(TransportCarrier::Tcp)?,
            })
        };
        let configs = rebuild_configs()?;
        let (server_config, client_config) = (configs.server, configs.client);

        // Bind socket через primitives' helper, build endpoint вручную
        // чтобы set'нуть и server и client config.
        let runtime = Arc::new(TokioRuntime);
        // Bottom byte-pipe per the chosen carrier (see docs/decisions/connection-quality-ladder.md +
        // tcp-relay-fallback.md):
        // - UDP: the native socket (rungs 1–2).
        // - TCP relay: a `TcpQuicSocket` (rung 0) — QUIC inside a TLS-over-TCP connection.
        let carrier_kind = match &choice {
            CarrierChoice::TcpRelay(_) => InterfaceKind::QuicOverTcp,
            CarrierChoice::Udp(_) => InterfaceKind::QuicOverUdp,
        };
        // The native-UDP carrier rides a `MuxRouter` so a peer's ONE quinn connection can be switched
        // between the UDP base and its direct-TCP door by QUIC migration (`mux`); pure-TCP carriers
        // (relay / legacy direct) do not (they ARE the byte-pipe).
        let mut mux: Option<Arc<MuxRouter>> = None;
        let mut stun: Option<Arc<StunSocket>> = None;
        let abstract_socket: Arc<dyn quinn::AsyncUdpSocket> = match choice {
            // The relay's TLS-over-TCP byte-pipe (rung 0). A peer's own direct-TCP door is no longer a
            // separate carrier — it's a mux route on the UDP carrier (Meta-B migration).
            CarrierChoice::TcpRelay(tcp_addr) => {
                crate::tcp_transport::TcpQuicSocket::connect(tcp_addr, OMNI_SNI)?
                    as Arc<dyn quinn::AsyncUdpSocket>
            }
            CarrierChoice::Udp(bind_addr) => {
                let socket = bind_quic_udp_socket(bind_addr)
                    .map_err(|e| io::Error::other(format!("udp socket: {e}")))?;
                let raw_socket = runtime.wrap_udp_socket(socket)?;
                // STUN rides the raw socket: STUN servers speak plain STUN.
                let stun_socket = StunSocket::new(raw_socket)?;
                stun = Some(Arc::clone(&stun_socket));
                let base: Arc<dyn quinn::AsyncUdpSocket> = stun_socket;
                let router = MuxRouter::new(base)?;
                mux = Some(Arc::clone(&router));
                router as Arc<dyn quinn::AsyncUdpSocket>
            }
        };
        // What the socket can ACTUALLY batch, once per endpoint. Segmentation offload is the whole
        // ballgame on this path — a profile put 98% of the process's CPU in ntdll and the kernel, so
        // throughput tracks syscalls-per-byte, and the one change that ever moved it was letting the
        // socket batch (GSO). `tx` is send-side segments per syscall, `rx` is receive-side. A `1` here
        // means that direction is paying one syscall per packet and is the next thing worth fixing.
        tcp_trace(
            "quic/offload",
            &format!(
                "kind={:?} max_transmit_segments={} max_receive_segments={}",
                carrier_kind,
                abstract_socket.max_transmit_segments(),
                abstract_socket.max_receive_segments(),
            ),
        );
        // The endpoint must be willing to RECEIVE what the other side's MTU discovery may send, or
        // probing stops at quinn's 1452 default no matter what the path can carry.
        let mut endpoint_config = EndpointConfig::default();
        endpoint_config
            .max_udp_payload_size(omni_net_primitives::quic::QUIC_MAX_UDP_PAYLOAD_BYTES)
            .map_err(|error| io::Error::other(format!("max_udp_payload_size: {error}")))?;
        let mut endpoint = Endpoint::new_with_abstract_socket(
            endpoint_config,
            Some(server_config),
            abstract_socket,
            runtime,
        )?;
        endpoint.set_default_client_config(client_config.clone());

        let local_addr = endpoint.local_addr()?;
        let carrier_tag = match carrier_kind {
            InterfaceKind::QuicOverTcp => "tcp",
            _ => "udp",
        };
        let id = InterfaceId(format!("quic-{carrier_tag}-{local_addr}"));

        let (inbound_tx, inbound_rx) = mpsc::channel(1024);
        let (datagram_tx, datagram_rx) = mpsc::channel(1024);
        let (closed_tx, closed_rx) = mpsc::channel(256);
        // Bulk plane: one entry per INCOMING transfer, not per packet — a tiny queue is plenty.
        let (bulk_tx, bulk_rx) = mpsc::channel(32);

        let iface = Arc::new(Self {
            id,
            kind: carrier_kind,
            endpoint: endpoint.clone(),
            inbound_rx: AsyncMutex::new(inbound_rx),
            connections: Arc::new(SyncMutex::new(HashMap::new())),
            inbound_tx: inbound_tx.clone(),
            datagram_rx: AsyncMutex::new(datagram_rx),
            datagram_tx: datagram_tx.clone(),
            closed_rx: AsyncMutex::new(closed_rx),
            closed_tx: closed_tx.clone(),
            bulk_rx: AsyncMutex::new(bulk_rx),
            bulk_tx: bulk_tx.clone(),
            local_is_ipv6: local_addr.is_ipv6(),
            mux,
            stun,
            client_config: SyncMutex::new(client_config),
            door_client_config: SyncMutex::new(configs.door_client),
            door_server_config: SyncMutex::new(Arc::new(configs.door_server)),
            rebuild_configs: Box::new(rebuild_configs),
        });

        // Spawn accept loop: новые incoming connections обрабатываются.
        {
            let iface_clone = Arc::clone(&iface);
            tokio::spawn(async move {
                while let Some(incoming) = endpoint.accept().await {
                    let from = incoming.remote_address();
                    tcp_trace("quic/incoming", &format!("from={from}"));
                    let iface = Arc::clone(&iface_clone);
                    // In from our own door's bridge: the connection rides TCP, it gets the door's
                    // config (queue budget).
                    let accepted = if crate::tcp_transport::is_door_bridge_source(from) {
                        let config = Arc::clone(&iface.door_server_config.lock());
                        incoming.accept_with(config)
                    } else {
                        incoming.accept()
                    };
                    tokio::spawn(async move {
                        let connected = match accepted {
                            Ok(connecting) => connecting.await,
                            Err(e) => Err(e),
                        };
                        match connected {
                            Ok(conn) => {
                                spawn_conn_state_trace(conn.clone(), "in");
                                let neighbor =
                                    connection_neighbor_id(conn.remote_address(), &conn);
                                iface.adopt_connection(conn, neighbor);
                            }
                            Err(e) => {
                                tcp_trace("quic/accept-fail", &format!("from={from} err={e}"));
                                eprintln!("quic accept error: {e}");
                            }
                        }
                    });
                }
            });
        }

        Ok(iface)
    }

    /// Применить СМЕНУ контроллера перегрузки на живом узле.
    ///
    /// Зачем отдельный путь: контроллер выбирается при построении `TransportConfig`, а тот
    /// вмораживается в конфиги endpoint'а — то есть без пересборки настройка не подействовала бы
    /// вообще. Раньше оболочка добивалась этого перезапуском меша, и это стоило **больше минуты
    /// без связи**: перезапуск сносит не только сокеты, а ещё дискавери, анонсы, проброс порта и
    /// заставляет заново проходить рандеву (живьём 2026-08-31: `node_endpoints=none`,
    /// `link=searching` дольше 60 с).
    ///
    /// Здесь пересобираются только конфиги, а существующие соединения закрываются: quinn не умеет
    /// менять контроллер на живом соединении, поэтому старое обязано уйти — иначе настройка
    /// «применилась» бы только на бумаге. Меш увидит закрытие своим `closed` планом и передозвонится
    /// обычным путём.
    ///
    /// Возвращает, сколько соединений закрыто — ноль означает «менять было нечего», и вызывающий
    /// не должен принимать это за успешное применение к живому линку.
    pub fn refresh_congestion_control(&self) -> io::Result<usize> {
        let configs = (self.rebuild_configs)()?;
        self.endpoint.set_server_config(Some(configs.server));
        *self.client_config.lock() = configs.client;
        *self.door_client_config.lock() = configs.door_client;
        *self.door_server_config.lock() = Arc::new(configs.door_server);

        let closed: Vec<quinn::Connection> = {
            let mut conns = self.connections.lock();
            conns.drain().map(|(_, conn)| conn).collect()
        };
        for conn in &closed {
            conn.close(0u32.into(), b"congestion-control-changed");
        }
        Ok(closed.len())
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    /// Map an IPv4 dial target to its v4-mapped IPv6 form (`::ffff:a.b.c.d`) when our socket
    /// is dual-stack IPv6 — the OS then sends it as a normal IPv4 packet. A no-op for a v4
    /// socket or an already-v6 target, so the proven IPv4 dial path is unchanged.
    fn mapped_dial_target(&self, addr: SocketAddr) -> SocketAddr {
        match addr.ip() {
            std::net::IpAddr::V4(v4) if self.local_is_ipv6 => {
                SocketAddr::new(std::net::IpAddr::V6(v4.to_ipv6_mapped()), addr.port())
            }
            _ => addr,
        }
    }

    /// Connection to `addr`: a live one we already hold (dialed OR accepted — the same peer socket
    /// is the same peer, libp2p's "dial unless connected"), else a fresh dial. The returned
    /// [`NeighborId`] names that ONE connection (`connection_neighbor_id`).
    ///
    /// Phase α-1 fix (F2): **outbound dials также spawn'ят recv loop** (через `adopt_connection`,
    /// как и принятые) — иначе набравший мог бы слать, но не получать.
    pub async fn dial(&self, addr: SocketAddr) -> io::Result<NeighborId> {
        // Dial the v4-mapped form when our socket is dual-stack IPv6 (else quinn can't route a v4
        // target). No-op for v4. An accepted connection reports the mapped form too.
        let target = self.mapped_dial_target(addr);
        {
            // Reuse only a LIVE connection. A dead one — the peer/relay restarted, or its NAT
            // mapping dropped — must not be handed back: it silently drops every send, and a
            // re-dial after a relay PROCESS restart never reconnected (the warm-rejoin black-hole,
            // docs/bugs/relay-pairing-approval-gap.md). A closed connection removes itself
            // (`adopt_connection`); the `close_reason()` check covers the moment in between.
            let conns = self.connections.lock();
            let live = conns.iter().find(|(_, conn)| {
                conn.close_reason().is_none()
                    && (conn.remote_address() == target || conn.remote_address() == addr)
            });
            if let Some((neighbor, _)) = live {
                return Ok(neighbor.clone());
            }
        }
        // `connect_with`, а не `connect`: конфиг берём СВОЙ, чтобы смена контроллера перегрузки
        // подействовала со следующего дозвона, без пересоздания endpoint'а и перезапуска меша.
        // Routed into the peer's door by the mux: the connection rides TCP, the door's config.
        let on_door = self.mux.as_ref().is_some_and(|mux| mux.is_on_door(target));
        let client_config =
            if on_door { self.door_client_config.lock().clone() } else { self.client_config.lock().clone() };
        let connecting = self
            .endpoint
            .connect_with(client_config, target, OMNI_SNI)
            .map_err(|e| io::Error::other(format!("connect init: {e}")))?;
        let conn = connecting
            .await
            .map_err(|e| io::Error::other(format!("connect: {e}")))?;
        spawn_conn_state_trace(conn.clone(), "out");
        let neighbor = connection_neighbor_id(addr, &conn);
        self.adopt_connection(conn, neighbor.clone());
        Ok(neighbor)
    }

    /// Put a fresh connection (dialed or accepted — the two paths are identical from here) into
    /// `connections` and run its three planes — frames, datagrams (media), bulk (attachments) —
    /// for its life. When it closes, the frame plane reports it on `closed_tx` and the entry is
    /// removed: with a key per connection nothing ever overwrites it, and a dead handle kept here
    /// would pin the connection's state for the life of the node.
    fn adopt_connection(&self, conn: quinn::Connection, neighbor: NeighborId) {
        self.connections.lock().insert(neighbor.clone(), conn.clone());
        tokio::spawn(Self::datagram_loop(
            conn.clone(),
            neighbor.clone(),
            self.datagram_tx.clone(),
        ));
        // Either side may open a transfer, so the acceptor runs on dialed connections too.
        tokio::spawn(Self::bulk_loop(
            conn.clone(),
            neighbor.clone(),
            self.bulk_tx.clone(),
        ));
        let connections = Arc::clone(&self.connections);
        let (inbound_tx, closed_tx) = (self.inbound_tx.clone(), self.closed_tx.clone());
        tokio::spawn(async move {
            Self::recv_loop(conn, neighbor.clone(), inbound_tx, closed_tx).await;
            connections.lock().remove(&neighbor);
        });
    }

    /// Process incoming streams from a single connection until it closes. On
    /// close, emit the neighbour on `closed_tx` so the mesh prunes its path
    /// (P2 cleanup-on-close).
    async fn recv_loop(
        conn: quinn::Connection,
        neighbor: NeighborId,
        inbound_tx: mpsc::Sender<(NeighborId, Vec<u8>)>,
        closed_tx: mpsc::Sender<NeighborId>,
    ) {
        loop {
            match conn.accept_uni().await {
                Ok(mut stream) => {
                    let neighbor = neighbor.clone();
                    let inbound_tx = inbound_tx.clone();
                    tokio::spawn(async move {
                        let mut buf = Vec::new();
                        match stream.read_to_end(64 * 1024).await {
                            Ok(bytes) => {
                                buf.extend_from_slice(&bytes);
                                let _ = inbound_tx.send((neighbor, buf)).await;
                            }
                            Err(e) => {
                                eprintln!("recv stream error: {e}");
                            }
                        }
                    });
                }
                Err(e) => {
                    // Connection closed; tell the mesh to prune this path, then exit.
                    eprintln!("conn closed: {e}");
                    let _ = closed_tx.try_send(neighbor);
                    break;
                }
            }
        }
    }

    /// Accept inbound bulk streams (attachment transfer) on a connection until it closes.
    ///
    /// Each stream announces itself with [`BULK_STREAM_PREFACE`] before any payload, so a stream
    /// opened for some other future purpose on this connection is rejected here instead of being
    /// mis-fed to the transfer reader. Reading the preface is also what makes the accept loop
    /// yield only streams whose opener has actually started talking.
    async fn bulk_loop(
        conn: quinn::Connection,
        neighbor: NeighborId,
        bulk_tx: mpsc::Sender<(NeighborId, BulkStream)>,
    ) {
        // Ends when the connection closes — the frame plane's recv_loop reports the prune.
        while let Ok((send, mut recv)) = conn.accept_bi().await {
            let neighbor = neighbor.clone();
            let bulk_tx = bulk_tx.clone();
            // Per-stream task: a peer opening a second transfer must not wait behind the
            // preface read of the first.
            tokio::spawn(async move {
                let mut preface = [0_u8; BULK_STREAM_PREFACE.len()];
                if recv.read_exact(&mut preface).await.is_err() {
                    return;
                }
                if preface != BULK_STREAM_PREFACE {
                    tcp_trace(
                        "quic/bulk-reject",
                        &format!("neighbor={} bad preface", neighbor.0),
                    );
                    return;
                }
                tcp_trace("quic/bulk-accept", &format!("neighbor={}", neighbor.0));
                let stream = BulkStream {
                    send: Box::new(send),
                    recv: Box::new(recv),
                };
                let _ = bulk_tx.send((neighbor, stream)).await;
            });
        }
    }

    /// Read unreliable datagrams from a single connection until it closes,
    /// forwarding each to the shared datagram channel tagged with `neighbor`.
    /// Raw bytes — no NetworkFrame header (the connection identifies the peer).
    async fn datagram_loop(
        conn: quinn::Connection,
        neighbor: NeighborId,
        datagram_tx: mpsc::Sender<(NeighborId, Vec<u8>)>,
    ) {
        // `read_datagram` erroring = connection closed → stop the pump.
        while let Ok(bytes) = conn.read_datagram().await {
            if datagram_tx
                .send((neighbor.clone(), bytes.to_vec()))
                .await
                .is_err()
            {
                break; // receiver dropped
            }
        }
    }
}

/// Upper bound on how long a stopping node waits for its CONNECTION_CLOSE frames to flush. The
/// close frame goes out on the first driver turn; the bound only stops a congested or unreachable
/// path from holding up app shutdown.
const QUIC_SHUTDOWN_FLUSH: Duration = Duration::from_millis(300);

#[async_trait]
impl Interface for QuicOverUdpInterface {
    fn id(&self) -> InterfaceId {
        self.id.clone()
    }

    fn kind(&self) -> InterfaceKind {
        self.kind
    }

    fn mtu(&self) -> usize {
        // QUIC frames typically up to ~1200 bytes payload after overhead.
        1200
    }

    fn bandwidth_estimate(&self) -> u64 {
        // Local UDP: assume gigabit. Real implementation would measure.
        1_000_000_000
    }

    fn latency_estimate(&self) -> Duration {
        // LAN-ish default. Real impl measures via ping frames.
        Duration::from_millis(5)
    }

    fn reliable(&self) -> bool {
        true
    }

    fn broadcast_capable(&self) -> bool {
        false
    }

    async fn connect_endpoint(&self, endpoint: &str) -> io::Result<NeighborId> {
        let addr: SocketAddr = endpoint.parse().map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not a socket addr {endpoint:?}: {e}"),
            )
        })?;
        self.dial(addr).await
    }

    async fn send_frame(&self, neighbor: &NeighborId, frame: &[u8]) -> io::Result<()> {
        let conn = self
            .connections
            .lock()
            .get(neighbor)
            .cloned()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!("no connection to {neighbor:?}"),
                )
            })?;
        let mut stream = conn
            .open_uni()
            .await
            .map_err(|e| io::Error::other(format!("open_uni: {e}")))?;
        stream
            .write_all(frame)
            .await
            .map_err(|e| io::Error::other(format!("write: {e}")))?;
        stream
            .finish()
            .map_err(|e| io::Error::other(format!("finish: {e}")))?;
        Ok(())
    }

    async fn recv_frame(&self) -> io::Result<(NeighborId, Vec<u8>)> {
        let mut rx = self.inbound_rx.lock().await;
        rx.recv()
            .await
            .ok_or_else(|| io::Error::other("inbound channel closed"))
    }

    async fn broadcast_frame(&self, frame: &[u8]) -> io::Result<()> {
        // QUIC is point-to-point — итерируем по известным connections.
        let conns: Vec<_> = self
            .connections
            .lock()
            .iter()
            .map(|(n, c)| (n.clone(), c.clone()))
            .collect();
        for (_neighbor, conn) in conns {
            let mut stream = match conn.open_uni().await {
                Ok(s) => s,
                Err(_) => continue, // dead connection — skip
            };
            let _ = stream.write_all(frame).await;
            let _ = stream.finish();
        }
        Ok(())
    }

    fn supports_datagrams(&self) -> bool {
        true
    }

    fn remote_addr(&self, neighbor: &NeighborId) -> Option<std::net::SocketAddr> {
        self.connections
            .lock()
            .get(neighbor)
            .map(|c| c.remote_address())
    }

    fn connection_rx_bytes(&self, neighbor: &NeighborId) -> Option<u64> {
        self.connections
            .lock()
            .get(neighbor)
            .map(|c| c.stats().udp_rx.bytes)
    }

    fn refresh_congestion_control(&self) -> io::Result<usize> {
        QuicOverUdpInterface::refresh_congestion_control(self)
    }

    fn connection_rtt(&self, neighbor: &NeighborId) -> Option<std::time::Duration> {
        self.connections
            .lock()
            .get(neighbor)
            .map(|c| c.stats().path.rtt)
    }

    async fn route_peer_over_door(&self, peer_quic: SocketAddr, door_tcp: SocketAddr) -> io::Result<()> {
        match &self.mux {
            Some(mux) => mux.route_over_door(peer_quic, door_tcp, OMNI_SNI).await,
            None => Err(io::Error::other("interface has no mux router (pure-TCP carrier)")),
        }
    }

    fn route_peer_over_udp(&self, peer_quic: SocketAddr) {
        if let Some(mux) = &self.mux {
            mux.route_over_udp(peer_quic);
        }
    }

    fn peer_on_door(&self, peer_quic: SocketAddr) -> bool {
        self.mux.as_ref().map(|m| m.is_on_door(peer_quic)).unwrap_or(false)
    }

    fn peer_door_of(&self, peer_quic: SocketAddr) -> Option<SocketAddr> {
        self.mux.as_ref().and_then(|m| m.door_of(peer_quic))
    }

    async fn stun_reflexive(
        &self,
        servers: &[&str],
        timeout: std::time::Duration,
    ) -> Option<omni_net_primitives::stun::Reflexive> {
        self.stun.as_ref()?.reflexive(servers, timeout).await
    }

    async fn send_datagram(&self, neighbor: &NeighborId, payload: &[u8]) -> io::Result<()> {
        let conn = self
            .connections
            .lock()
            .get(neighbor)
            .cloned()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!("no connection to {neighbor:?}"),
                )
            })?;
        // Прибор очереди датаграмм quinn — участка между нашим пейсером и проводом, за которым
        // до 2026-09-24 не смотрел никто: quinn при нехватке места МОЛЧА выбрасывает самую старую
        // датаграмму и возвращает Ok. Здесь видно и то, и другое: сколько в очереди стояло, и
        // сколько раз места не хватило. См. docs/bugs/datagrams-we-counted-as-sent-never-left.md.
        let capacity = omni_net_primitives::quic::datagram_send_buffer_bytes();
        if conn.datagram_send_buffer_space() < payload.len() {
            DATAGRAM_SEND_OVERFLOWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let sent = conn
            .send_datagram(Bytes::copy_from_slice(payload))
            .map_err(|e| io::Error::other(format!("send_datagram: {e}")));
        let queued = capacity.saturating_sub(conn.datagram_send_buffer_space());
        DATAGRAM_QUEUE_PEAK_BYTES.fetch_max(queued, std::sync::atomic::Ordering::Relaxed);
        sent
    }

    async fn recv_datagram(&self) -> io::Result<(NeighborId, Vec<u8>)> {
        let mut rx = self.datagram_rx.lock().await;
        rx.recv()
            .await
            .ok_or_else(|| io::Error::other("datagram channel closed"))
    }

    async fn close(&self) {
        // quinn's own shutdown: CONNECTION_CLOSE to every peer, then wait (bounded) for the
        // endpoint to go idle so the close frames actually leave before the runtime is torn down.
        self.endpoint.close(0u32.into(), b"shutdown");
        let _ = tokio::time::timeout(QUIC_SHUTDOWN_FLUSH, self.endpoint.wait_idle()).await;
    }

    async fn close_neighbor(&self, neighbor: &NeighborId) {
        let conn = self.connections.lock().get(neighbor).cloned();
        if let Some(conn) = conn {
            conn.close(0u32.into(), b"direct-off");
        }
    }

    async fn recv_closed(&self) -> io::Result<NeighborId> {
        let mut rx = self.closed_rx.lock().await;
        rx.recv()
            .await
            .ok_or_else(|| io::Error::other("closed channel closed"))
    }

    fn supports_bulk_streams(&self) -> bool {
        true
    }

    async fn open_bulk_stream(&self, neighbor: &NeighborId) -> io::Result<BulkStream> {
        let conn = self
            .connections
            .lock()
            .get(neighbor)
            .cloned()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!("no connection to {neighbor:?}"),
                )
            })?;
        // A stream on the EXISTING connection: no dial, no handshake, and it inherits whatever
        // carrier that connection currently rides (incl. a door it migrated onto).
        let (mut send, recv) = conn
            .open_bi()
            .await
            .map_err(|e| io::Error::other(format!("open_bi: {e}")))?;
        send.write_all(&BULK_STREAM_PREFACE)
            .await
            .map_err(|e| io::Error::other(format!("bulk preface: {e}")))?;
        tcp_trace("quic/bulk-open", &format!("neighbor={}", neighbor.0));
        Ok(BulkStream {
            send: Box::new(send),
            recv: Box::new(recv),
        })
    }

    async fn accept_bulk_stream(&self) -> io::Result<(NeighborId, BulkStream)> {
        let mut rx = self.bulk_rx.lock().await;
        rx.recv()
            .await
            .ok_or_else(|| io::Error::other("bulk channel closed"))
    }
}

/// The [`NeighborId`] of ONE QUIC connection: the peer's address plus quinn's per-endpoint
/// `stable_id`.
///
/// The address alone was the key before, and it named a PEER SOCKET, not a connection. A
/// simultaneous open (both sides dial at once — the punch, a reconnect race) yields TWO connections
/// on one 4-tuple; under one key they collapsed into one `connections` entry and one path, so the
/// mesh's "both sides pick the same connection" tie-break (`best_preferring`) had nothing to choose
/// from and each side sent over whichever it registered last. Worse, when the spare one closed, its
/// close was reported under the shared key and `prune_path` tore down the LIVE one's routing with
/// it. See docs/bugs/two-connections-shared-one-neighbour-id.md.
///
/// The address stays in the id for traces only — nothing parses it back.
fn connection_neighbor_id(addr: SocketAddr, conn: &quinn::Connection) -> NeighborId {
    NeighborId(format!("{addr}#{}", conn.stable_id()))
}

/// Per-connection quinn-state monitor for the `OMNI_TRACE_FILE` diagnostic sink (not spawned
/// when it is off). Emits `quic/conn-open` (dir=in|out), `quic/conn-closed` with quinn's
/// `close_reason` + age (distinguishes an idle-timeout from a peer close), `quic/path-migrated`
/// on a remote-address change (carrier switch UDP↔door is a QUIC path migration), and
/// `quic/path-frames` when PATH_CHALLENGE/PATH_RESPONSE counters move (did the migration
/// path-validate?). Needed to see, on the door owner's side, whether a throttled peer's
/// connection ever establishes / idle-times-out / migrates — see
/// docs/bugs/relay-less-mobile-transport-findings.md §«REFINED on the live stand».
fn spawn_conn_state_trace(conn: quinn::Connection, dir: &'static str) {
    if !trace_enabled() {
        return;
    }
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let mut remote = conn.remote_address();
        tcp_trace("quic/conn-open", &format!("dir={dir} remote={remote}"));
        let (mut ch_rx, mut ch_tx, mut re_rx, mut re_tx) = (0u64, 0u64, 0u64, 0u64);
        let mut last_stat_sent = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Some(reason) = conn.close_reason() {
                let s = conn.stats();
                tcp_trace(
                    "quic/conn-closed",
                    &format!(
                        "dir={dir} remote={remote} age_s={} reason=\"{reason}\" \
                         udp_rx={}p/{}B udp_tx={}p/{}B hs_done_rx={}",
                        started.elapsed().as_secs(),
                        s.udp_rx.datagrams,
                        s.udp_rx.bytes,
                        s.udp_tx.datagrams,
                        s.udp_tx.bytes,
                        s.frame_rx.handshake_done,
                    ),
                );
                tcp_trace(
                    "quic/stats",
                    &quic_stats_line(dir, remote, &s, true, bdp_cap_snapshot(&conn), DatagramQueueSnapshot::take()),
                );
                break;
            }
            let now = conn.remote_address();
            if now != remote {
                tcp_trace(
                    "quic/path-migrated",
                    &format!(
                        "dir={dir} old={remote} new={now} age_s={}",
                        started.elapsed().as_secs()
                    ),
                );
                remote = now;
            }
            let s = conn.stats();
            let (nch_rx, nch_tx) = (s.frame_rx.path_challenge, s.frame_tx.path_challenge);
            let (nre_rx, nre_tx) = (s.frame_rx.path_response, s.frame_tx.path_response);
            if (nch_rx, nch_tx, nre_rx, nre_tx) != (ch_rx, ch_tx, re_rx, re_tx) {
                tcp_trace(
                    "quic/path-frames",
                    &format!(
                        "dir={dir} remote={now} challenge_rx={nch_rx} challenge_tx={nch_tx} \
                         response_rx={nre_rx} response_tx={nre_tx} age_s={}",
                        started.elapsed().as_secs()
                    ),
                );
                (ch_rx, ch_tx, re_rx, re_tx) = (nch_rx, nch_tx, nre_rx, nre_tx);
            }
            // Loss/goodput snapshot, emitted every ~256 sent packets (silent when idle) so a
            // capture shows the real retransmit rate on the throttled uplink, not just byte totals.
            if s.path.sent_packets.saturating_sub(last_stat_sent) >= 256 {
                tcp_trace(
                    "quic/stats",
                    &quic_stats_line(dir, now, &s, false, bdp_cap_snapshot(&conn), DatagramQueueSnapshot::take()),
                );
                last_stat_sent = s.path.sent_packets;
            }
        }
    });
}

/// One-line loss/congestion snapshot from quinn's `ConnectionStats`. `loss_pct` is the share of
/// sent packets declared lost (i.e. retransmitted) — the direct read on a policed carrier.
///
/// `blocked=<conn>/<stream>` counts DATA_BLOCKED / STREAM_DATA_BLOCKED frames SENT: quinn emits one
/// only when it has bytes ready and the PEER'S flow-control window forbids sending them. That is the
/// one signal that separates the two reasons a sender can be slow while the wire is idle — the
/// congestion controller holding it back (cwnd, visible above) versus the receiver's window holding
/// it back (here). Both look identical from a stopwatch, and guessing between them cost real time.
/// Пик заполнения очереди исходящих датаграмм quinn и число отправок, при которых места не хватило
/// (quinn тогда выбрасывает самую старую датаграмму), — с прошлой строки `quic/stats`. На процесс, а
/// не на соединение: медиа идёт одним соединением.
static DATAGRAM_QUEUE_PEAK_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static DATAGRAM_SEND_OVERFLOWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The two counters above, read and reset by the stats emitter — the formatter stays pure.
#[derive(Clone, Copy, Debug, Default)]
struct DatagramQueueSnapshot {
    peak_bytes: usize,
    overflows: u64,
}

impl DatagramQueueSnapshot {
    fn take() -> Self {
        Self {
            peak_bytes: DATAGRAM_QUEUE_PEAK_BYTES.swap(0, std::sync::atomic::Ordering::Relaxed),
            overflows: DATAGRAM_SEND_OVERFLOWS.swap(0, std::sync::atomic::Ordering::Relaxed),
        }
    }
}

fn quic_stats_line(
    dir: &str,
    remote: SocketAddr,
    s: &quinn::ConnectionStats,
    final_: bool,
    bdp_cap: Option<BdpCapSnapshot>,
    datagram_queue: DatagramQueueSnapshot,
) -> String {
    let p = &s.path;
    let loss = if p.sent_packets > 0 {
        p.lost_packets as f64 * 100.0 / p.sent_packets as f64
    } else {
        0.0
    };
    format!(
        "dir={dir} remote={remote}{} rtt_ms={} cwnd={} sent_pkts={} lost_pkts={} loss_pct={loss:.2} \
         udp_tx={}B udp_rx={}B lost_bytes={} cong={} mtu={} blackholes={} blocked={}/{} \
         dgram_q_peak={}B dgram_overflow={}{}",
        if final_ { " FINAL" } else { "" },
        p.rtt.as_millis(),
        p.cwnd,
        p.sent_packets,
        p.lost_packets,
        s.udp_tx.bytes,
        s.udp_rx.bytes,
        p.lost_bytes,
        p.congestion_events,
        p.current_mtu,
        p.black_holes_detected,
        s.frame_tx.data_blocked,
        s.frame_tx.stream_data_blocked,
        datagram_queue.peak_bytes,
        datagram_queue.overflows,
        bdp_cap.map(bdp_cap_fields).unwrap_or_default(),
    )
}

/// The BDP cap's own state, appended to the stats line when the cap is installed — on a connection
/// over UDP by default (`UDP_QUEUE_BUDGET`), off with `OMNI_QUIC_QUEUE_BUDGET_MS=off`.
///
/// `bdp_cap=<cap>/<uncapped>` is the whole point: equal values mean the cap never bound and the run
/// says nothing about it, a much smaller left number means Cubic was held back and by how much. The
/// two inputs follow so a suspicious cap can be blamed on the right estimate.
fn bdp_cap_fields(snapshot: BdpCapSnapshot) -> String {
    let cap = match snapshot.cap {
        Some(cap) => cap.to_string(),
        // Not enough of the path measured yet to judge; the cap deliberately does nothing here.
        None => "none".to_string(),
    };
    format!(
        " bdp_cap={cap}/{} cap_min_rtt_us={} cap_rate_mbit={:.2}",
        snapshot.uncapped_window,
        // Microseconds, not milliseconds: on a fast path the whole input rounds to "0 ms", which
        // hides the one number that explains why the cap collapsed to its floor.
        snapshot
            .min_rtt
            .map(|rtt| rtt.as_micros().to_string())
            .unwrap_or_else(|| "none".to_string()),
        snapshot.delivery_rate_bytes_per_sec * 8.0 / 1_000_000.0,
    )
}

/// Cheap only because it runs behind `OMNI_TRACE_FILE` and at most once per stats line: quinn hands
/// out the controller by cloning it under the connection lock.
fn bdp_cap_snapshot(conn: &quinn::Connection) -> Option<BdpCapSnapshot> {
    omni_net_primitives::bdp_cap::snapshot_from_controller(conn.congestion_state())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;
    use std::net::{IpAddr, Ipv4Addr};

    fn loopback(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port)
    }

    fn fresh_signing_key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    /// Смена контроллера обязана ЗАКРЫТЬ живое соединение: quinn не умеет менять контроллер на
    /// поднятом соединении, поэтому оставить его — значит применить настройку только на бумаге.
    #[tokio::test]
    async fn changing_the_controller_drops_the_connections_that_still_ride_the_old_one() {
        let a = QuicOverUdpInterface::bind(loopback(0), &fresh_signing_key())
            .expect("bind a");
        let b = QuicOverUdpInterface::bind(loopback(0), &fresh_signing_key())
            .expect("bind b");

        // Отрицательная половина, и она первая: пока соединений нет, закрывать нечего. Без неё
        // «закрыли >0» могло бы выполняться по любой причине.
        assert_eq!(
            a.refresh_congestion_control().expect("refresh on idle"),
            0,
            "на узле без соединений закрывать нечего"
        );

        let b_addr = b.local_addr().expect("b addr");
        a.dial(b_addr).await.expect("dial b");
        assert_eq!(a.connections.lock().len(), 1, "дозвон должен дать соединение");

        assert_eq!(
            a.refresh_congestion_control().expect("refresh with a link"),
            1,
            "живое соединение обязано быть закрыто, иначе оно доедет на старом контроллере"
        );
        assert!(
            a.connections.lock().is_empty(),
            "закрытые соединения не должны оставаться в кэше — иначе следующий дозвон переиспользует их"
        );

        // И узел остаётся рабочим: дозвон после смены проходит заново.
        a.dial(b_addr).await.expect("re-dial after the change");
        assert_eq!(a.connections.lock().len(), 1);
    }

    #[test]
    fn the_stats_line_mentions_the_bdp_cap_only_when_the_cap_is_installed() {
        let stats = quinn::ConnectionStats::default();
        let remote = loopback(4433);

        let without =
            quic_stats_line("out", remote, &stats, false, None, DatagramQueueSnapshot::default());
        assert!(
            !without.contains("bdp_cap"),
            "a default build has no cap; its trace must not grow a field about one: {without}"
        );

        let with = quic_stats_line(
            "out",
            remote,
            &stats,
            false,
            Some(BdpCapSnapshot {
                uncapped_window: 2_097_152,
                cap: Some(245_760),
                min_rtt: Some(Duration::from_millis(44)),
                delivery_rate_bytes_per_sec: 2_300_000.0,
            }),
            DatagramQueueSnapshot::default(),
        );
        // The pair is the whole diagnostic: what the cap allowed vs what Cubic wanted.
        assert!(with.contains("bdp_cap=245760/2097152"), "{with}");
        assert!(with.contains("cap_min_rtt_us=44000"), "{with}");
        assert!(with.contains("cap_rate_mbit=18.40"), "{with}");
        assert!(
            with.starts_with(&without),
            "the cap fields must be appended, not woven in, so existing parsing keeps working"
        );
    }

    /// The datagram-queue instrument: what the emitter took is what the line says — peak bytes
    /// queued in quinn and how many sends found no room (quinn then drops the OLDEST datagram
    /// silently). A zero snapshot must print zeros, not vanish, so "never overflowed" is visible.
    #[test]
    fn the_stats_line_carries_the_datagram_queue_peak_and_overflows() {
        let stats = quinn::ConnectionStats::default();
        let quiet =
            quic_stats_line("out", loopback(4433), &stats, false, None, DatagramQueueSnapshot::default());
        assert!(quiet.contains("dgram_q_peak=0B dgram_overflow=0"), "{quiet}");
        let busy = quic_stats_line(
            "out",
            loopback(4433),
            &stats,
            false,
            None,
            DatagramQueueSnapshot { peak_bytes: 11_441, overflows: 3 },
        );
        assert!(busy.contains("dgram_q_peak=11441B dgram_overflow=3"), "{busy}");
    }

    #[test]
    fn an_unmeasured_path_says_so_instead_of_printing_a_cap_of_zero() {
        let line = quic_stats_line(
            "in",
            loopback(4433),
            &quinn::ConnectionStats::default(),
            false,
            Some(BdpCapSnapshot {
                uncapped_window: 12_000,
                cap: None,
                min_rtt: None,
                delivery_rate_bytes_per_sec: 0.0,
            }),
            DatagramQueueSnapshot::default(),
        );

        assert!(line.contains("bdp_cap=none/12000"), "{line}");
        assert!(line.contains("cap_min_rtt_us=none"), "{line}");
    }

    #[tokio::test]
    async fn bind_succeeds_on_ephemeral_port() {
        // H5: QUIC binding не требует special privileges и не конфликтует
        // с другими listeners — выбираем ephemeral port 0.
        let key = fresh_signing_key();
        let iface = QuicOverUdpInterface::bind(loopback(0), &key)
            .expect("bind should succeed on loopback");
        let local = iface.local_addr().unwrap();
        assert_eq!(local.ip(), Ipv4Addr::new(127, 0, 0, 1));
        assert!(local.port() != 0, "OS должна выдать non-zero port");
    }

    #[tokio::test]
    async fn id_includes_bound_address() {
        let key = fresh_signing_key();
        let iface = QuicOverUdpInterface::bind(loopback(0), &key).unwrap();
        let local = iface.local_addr().unwrap();
        let id_str = iface.id().0;
        assert!(id_str.starts_with("quic-udp-"));
        assert!(id_str.contains(&local.to_string()));
    }

    #[tokio::test]
    async fn explicit_constructors_report_their_carrier() {
        // Dual-carrier coexistence: a node can stand up one of each, and each reports its carrier
        // so `path_rank` ranks them (UDP rungs 1–2 over the TCP-relay rung 0).
        let key = fresh_signing_key();
        let udp = QuicOverUdpInterface::bind_udp(loopback(0), &key).unwrap();
        assert_eq!(udp.kind(), InterfaceKind::QuicOverUdp);
        assert!(udp.id().0.starts_with("quic-udp-"));

        // A bare TCP listener stands in for the relay bridge — `TcpQuicSocket::connect` only needs
        // the TCP connect to succeed to build the interface (no QUIC handshake required for `kind()`).
        let listener = tokio::net::TcpListener::bind(loopback(0)).await.unwrap();
        let relay_addr = listener.local_addr().unwrap();
        let tcp = QuicOverUdpInterface::bind_tcp_relay(relay_addr, &key).unwrap();
        assert_eq!(tcp.kind(), InterfaceKind::QuicOverTcp);
        assert!(tcp.id().0.starts_with("quic-tcp-"));
    }

    #[tokio::test]
    async fn send_frame_to_unknown_neighbor_returns_error() {
        let key = fresh_signing_key();
        let iface = QuicOverUdpInterface::bind(loopback(0), &key).unwrap();
        let result = iface
            .send_frame(&NeighborId("nobody".to_string()), &[1, 2, 3])
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotConnected);
    }

    /// Warm-rejoin: after a cached connection dies (peer/relay restart, NAT mapping
    /// drop), a re-dial of the SAME address must establish a FRESH connection instead
    /// of handing back the dead cached one. Regression for the relay-restart black-hole
    /// — `dial` reused a zombie connection, so every relayed send silently vanished and
    /// the mesh never reconnected to a restarted relay. See
    /// docs/bugs/relay-pairing-approval-gap.md.
    #[tokio::test]
    async fn redial_replaces_a_dead_cached_connection() {
        let alice_key = fresh_signing_key();
        let bob_key = fresh_signing_key();
        let alice = QuicOverUdpInterface::bind(loopback(0), &alice_key).unwrap();
        let bob = QuicOverUdpInterface::bind(loopback(0), &bob_key).unwrap();
        let bob_addr = bob.local_addr().unwrap();

        // First dial works.
        let n1 = alice.dial(bob_addr).await.expect("dial 1");
        alice.send_frame(&n1, b"first").await.expect("send 1");
        let (_, f1) = tokio::time::timeout(Duration::from_secs(3), bob.recv_frame())
            .await
            .expect("bob recv 1 within 3s")
            .expect("recv ok");
        assert_eq!(f1, b"first");

        // The cached connection dies (peer/relay went away).
        alice
            .connections
            .lock()
            .get(&n1)
            .expect("cached conn")
            .close(0u32.into(), b"bye");
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Re-dial the same address: must dial FRESH, not reuse the dead connection.
        // Without the fix this returns the zombie and the send below fails / never arrives.
        let n2 = alice.dial(bob_addr).await.expect("re-dial");
        alice
            .send_frame(&n2, b"after-restart")
            .await
            .expect("send over fresh conn");
        let got = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let (_, f) = bob.recv_frame().await.expect("recv ok");
                if f == b"after-restart" {
                    return f;
                }
            }
        })
        .await
        .expect("bob receives over the fresh connection within 3s");
        assert_eq!(got, b"after-restart");
    }

    /// Встречный одновременный дозвон даёт ДВА соединения на одном 4-кортеже. У каждого — свой
    /// `NeighborId`: по каждому ходят кадры, и закрытие лишнего сообщается только под его id.
    /// Раньше id был адресом собеседника, оба соединения делили его, и закрытие лишнего сносило
    /// маршрут живого. См. docs/bugs/two-connections-shared-one-neighbour-id.md.
    #[tokio::test]
    async fn a_simultaneous_open_gives_two_connections_two_ids() {
        let a = QuicOverUdpInterface::bind(loopback(0), &fresh_signing_key()).unwrap();
        let b = QuicOverUdpInterface::bind(loopback(0), &fresh_signing_key()).unwrap();
        let (a_addr, b_addr) = (a.local_addr().unwrap(), b.local_addr().unwrap());

        // Both dials look for a live connection before either handshake can finish, so each
        // opens its own.
        let (ab, ba) = tokio::join!(a.dial(b_addr), b.dial(a_addr));
        let (ab, ba) = (ab.expect("a dials b"), ba.expect("b dials a"));

        a.send_frame(&ab, b"over-ab").await.expect("a sends over its dial");
        let (ab_at_b, f) = tokio::time::timeout(Duration::from_secs(3), b.recv_frame())
            .await
            .expect("b receives within 3s")
            .expect("recv ok");
        assert_eq!(f, b"over-ab");
        b.send_frame(&ba, b"over-ba").await.expect("b sends over its dial");
        let (ba_at_a, f) = tokio::time::timeout(Duration::from_secs(3), a.recv_frame())
            .await
            .expect("a receives within 3s")
            .expect("recv ok");
        assert_eq!(f, b"over-ba");
        assert_ne!(ab, ba_at_a, "на A два соединения обязаны различаться");
        assert_ne!(ba, ab_at_b, "на B тоже");

        // Negative half: the spare connection closes — the close names only it, and the other
        // connection keeps carrying frames.
        a.connections.lock().get(&ab).expect("a holds its dial").close(0u32.into(), b"spare");
        let closed_at_b = tokio::time::timeout(Duration::from_secs(3), b.recv_closed())
            .await
            .expect("b learns of the close within 3s")
            .expect("closed ok");
        assert_eq!(closed_at_b, ab_at_b, "закрытие сообщается под id закрытого соединения");
        b.send_frame(&ba, b"still-here").await.expect("the other connection is alive");
        let (_, f) = tokio::time::timeout(Duration::from_secs(3), a.recv_frame())
            .await
            .expect("a receives over the survivor within 3s")
            .expect("recv ok");
        assert_eq!(f, b"still-here");
        tokio::time::timeout(Duration::from_secs(3), async {
            while a.connections.lock().contains_key(&ab) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a closed connection removes itself from the table");
        assert!(a.connections.lock().contains_key(&ba_at_a), "выжившее остаётся");
    }

    /// A stopping node closes its connections, so the peer learns at once and drops the path,
    /// instead of sending into a dead connection until its liveness prune (~20 s) — the lost
    /// receipt after a client restart (docs/bugs/receipt-after-restart-fails-to-decrypt.md).
    /// Negative half: while the node is up, the peer hears no close.
    #[tokio::test]
    async fn closing_the_interface_tells_the_peer_at_once() {
        let a = QuicOverUdpInterface::bind(loopback(0), &fresh_signing_key()).unwrap();
        let b = QuicOverUdpInterface::bind(loopback(0), &fresh_signing_key()).unwrap();
        let ab = a.dial(b.local_addr().unwrap()).await.expect("a dials b");
        a.send_frame(&ab, b"hello").await.expect("send");
        let (at_b, _) = tokio::time::timeout(Duration::from_secs(3), b.recv_frame())
            .await
            .expect("b receives within 3s")
            .expect("recv ok");

        assert!(
            tokio::time::timeout(Duration::from_millis(300), b.recv_closed()).await.is_err(),
            "a live peer is not reported closed"
        );

        a.close().await;
        let closed = tokio::time::timeout(Duration::from_secs(1), b.recv_closed())
            .await
            .expect("b learns of the close well before any idle timeout")
            .expect("closed ok");
        assert_eq!(closed, at_b);
    }

    /// Обратный дозвон по адресу, с которого к нам уже пришло живое соединение, берёт его, а не
    /// открывает второе ("dial unless connected"). Отрицательная половина: после его закрытия
    /// дозвон открывает новое, с другим id.
    #[tokio::test]
    async fn dialing_back_reuses_the_accepted_connection_while_it_lives() {
        let a = QuicOverUdpInterface::bind(loopback(0), &fresh_signing_key()).unwrap();
        let b = QuicOverUdpInterface::bind(loopback(0), &fresh_signing_key()).unwrap();
        let (a_addr, b_addr) = (a.local_addr().unwrap(), b.local_addr().unwrap());

        let ab = a.dial(b_addr).await.expect("a dials b");
        a.send_frame(&ab, b"hello").await.expect("send");
        let (accepted, _) = tokio::time::timeout(Duration::from_secs(3), b.recv_frame())
            .await
            .expect("b receives within 3s")
            .expect("recv ok");

        let back = b.dial(a_addr).await.expect("b dials back");
        assert_eq!(back, accepted, "живое принятое соединение переиспользуется");
        assert_eq!(b.connections.lock().len(), 1, "второе не открыто");

        b.connections.lock().get(&back).expect("held").close(0u32.into(), b"gone");
        let _ = tokio::time::timeout(Duration::from_secs(3), b.recv_closed()).await;
        let fresh = b.dial(a_addr).await.expect("b re-dials");
        assert_ne!(fresh, back, "мёртвое не возвращается — открыто новое соединение");
    }

    /// Phase α-1 F2 fix verification: outbound dial → bidirectional channel.
    /// Two interfaces, one dials the other, both can send + receive frames.
    #[tokio::test]
    async fn outbound_dial_supports_bidirectional_frames() {
        let alice_key = fresh_signing_key();
        let bob_key = fresh_signing_key();

        let alice = QuicOverUdpInterface::bind(loopback(0), &alice_key).unwrap();
        let bob = QuicOverUdpInterface::bind(loopback(0), &bob_key).unwrap();
        let bob_addr = bob.local_addr().unwrap();

        // Alice dials Bob.
        let bob_via_alice = alice.dial(bob_addr).await.expect("alice dials bob");

        // Alice sends → Bob receives.
        alice
            .send_frame(&bob_via_alice, b"hello bob")
            .await
            .expect("alice→bob send");

        let (alice_as_seen_by_bob, frame) = tokio::time::timeout(
            Duration::from_secs(3),
            bob.recv_frame(),
        )
        .await
        .expect("bob должен получить alice's frame в течение 3с")
        .expect("recv_frame не должен fail'нуть");
        assert_eq!(frame, b"hello bob");

        // Bob sends → Alice receives (this is the F2 fix path —
        // alice as outbound dialer должна также слушать).
        bob.send_frame(&alice_as_seen_by_bob, b"hello alice")
            .await
            .expect("bob→alice send");

        let (_, reply) = tokio::time::timeout(
            Duration::from_secs(3),
            alice.recv_frame(),
        )
        .await
        .expect("alice (outbound dialer) должна получить bob's reply — F2 fix")
        .expect("recv_frame не должен fail'нуть");
        assert_eq!(reply, b"hello alice");
    }

    #[tokio::test]
    async fn datagrams_flow_bidirectionally_over_dialed_connection() {
        // Media plane: once a connection exists, unreliable datagrams flow
        // both directions (opus/RTP rides this, not the reliable stream plane).
        let alice_key = fresh_signing_key();
        let bob_key = fresh_signing_key();

        let alice = QuicOverUdpInterface::bind(loopback(0), &alice_key).unwrap();
        let bob = QuicOverUdpInterface::bind(loopback(0), &bob_key).unwrap();
        assert!(alice.supports_datagrams() && bob.supports_datagrams());

        let bob_addr = bob.local_addr().unwrap();
        let bob_via_alice = alice.dial(bob_addr).await.expect("alice dials bob");

        // An opus-sized payload (≈ one 20 ms frame at 64 kbps).
        let media = vec![0xABu8; 160];
        alice
            .send_datagram(&bob_via_alice, &media)
            .await
            .expect("alice→bob datagram");

        let (alice_seen, got) =
            tokio::time::timeout(Duration::from_secs(3), bob.recv_datagram())
                .await
                .expect("bob receives datagram within 3s")
                .expect("recv_datagram ok");
        assert_eq!(got, media);

        // Reply on the same connection (outbound-dialer also receives — F2).
        bob.send_datagram(&alice_seen, b"pong")
            .await
            .expect("bob→alice datagram");
        let (_, reply) = tokio::time::timeout(Duration::from_secs(3), alice.recv_datagram())
            .await
            .expect("alice receives reply datagram within 3s")
            .expect("recv_datagram ok");
        assert_eq!(reply, b"pong");
    }
}
