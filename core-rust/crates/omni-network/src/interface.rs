// Link-layer abstraction. См. docs/architecture/networking-overlay.md
// §«Interface (link-layer) abstraction».
//
// Цель: routing/identity/announce не должны знать какой физический
// transport под капотом. Один trait, реализации:
// - QuicOverUdpInterface (point-to-point, primary v1)
// - MdnsInterface (LAN multicast, для discovery)
// - [future] QuicOverTcp, LoRa, Serial, etc.
//
// Это позволит позднее добавить multi-transport без переписывания
// routing layer'а — см. plan в networking-overlay.md.

use async_trait::async_trait;
use std::fmt::Debug;
use std::io;
use std::sync::Arc;
use std::time::Duration;

/// Stable identifier for an Interface instance (e.g. "quic-udp-0.0.0.0:8443"
/// or "mdns-en0"). Используется в path-table для пометки «через какой
/// interface пришёл announce».
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InterfaceId(pub String);

/// Kind of link-layer transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterfaceKind {
    QuicOverUdp,
    QuicOverTcp,
    Mdns,
    // Future:
    // LoRa,
    // Serial,
    // I2P,
}

/// Identifies a directly-connected neighbor on a specific interface.
/// Может быть IP:port для QUIC, MAC-like ID для radio, etc.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NeighborId(pub String);

/// A long-lived, ordered, flow-controlled byte pipe to ONE neighbour — the third plane next to
/// frames (reliable, per-message) and datagrams (unreliable, realtime media).
///
/// Why a separate plane: bulk payloads (attachment transfer) sent as mesh frames would pay
/// per-frame stream setup and, because each frame travels independently, arrive out of order —
/// forcing the receiver to hold the gaps in RAM, which does not scale to multi-gigabyte files.
/// A single ordered stream gives ordering, flow control and retransmission from the transport
/// itself, so the receiver can write straight through at a constant memory cost.
///
/// Deliberately expressed as boxed `AsyncRead`/`AsyncWrite` rather than a QUIC type: the mesh's
/// public surface stays transport-agnostic (a future radio/serial interface can carry bulk data
/// its own way), and no quinn types leak across crate boundaries.
pub struct BulkStream {
    pub send: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    pub recv: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
}

impl Debug for BulkStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BulkStream").finish_non_exhaustive()
    }
}

/// Bidirectional frame channel abstraction.
///
/// Frame = arbitrary bytes payload up to `mtu`. Routing layer
/// упаковывает announce'ы, path requests, virtual link data в frame'ы
/// и отдаёт Interface'у для transmission.
// `#[async_trait]` puts its own `#[must_use]` on every boxed future it returns, which clippy calls
// a double must-use; the attribute is the macro's, not ours.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait Interface: Debug + Send + Sync {
    /// Stable identifier для этого interface'а.
    fn id(&self) -> InterfaceId;

    /// Kind discriminator — для логирования / routing'а / приоритизации.
    fn kind(&self) -> InterfaceKind;

    /// Maximum bytes per frame для этого interface'а.
    /// QUIC ~1200 (after overhead), LoRa ~250, etc.
    fn mtu(&self) -> usize;

    /// Estimated bandwidth в bits per second. Используется для bandwidth-
    /// aware routing (preferring fat links over thin ones).
    fn bandwidth_estimate(&self) -> u64;

    /// Estimated one-way latency. Тоже для routing decisions.
    fn latency_estimate(&self) -> Duration;

    /// True если transport имеет встроенную reliable delivery (QUIC = yes,
    /// LoRa = no). Routing layer может избежать retry-логики если уже есть.
    fn reliable(&self) -> bool;

    /// True если physical layer broadcast'ится natively (LoRa = yes since
    /// air is shared, IP unicast = no, mDNS multicast = yes).
    fn broadcast_capable(&self) -> bool;

    /// Establish a connection to a peer reachable at `endpoint` (an
    /// interface-specific address string — `"1.2.3.4:443"` for QUIC,
    /// a hex node-id for a future radio link, etc). Returns the
    /// [`NeighborId`] to use with `send_frame`.
    ///
    /// Default: `Unsupported` — connectionless interfaces (multicast,
    /// LoRa-broadcast) discover neighbors passively via `recv_frame`
    /// rather than dialing. The mesh layer iterates interfaces and uses
    /// whichever one can connect to a peer's advertised endpoint.
    async fn connect_endpoint(&self, endpoint: &str) -> io::Result<NeighborId> {
        let _ = endpoint;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "interface is connectionless (no dial)",
        ))
    }

    /// Send frame to a specific neighbor. Returns error if neighbor
    /// disconnected или send failed.
    async fn send_frame(&self, neighbor: &NeighborId, frame: &[u8]) -> io::Result<()>;

    /// Receive next frame from any neighbor on this interface.
    /// Returns (neighbor_id, frame_bytes). Blocking — Interface
    /// owner вызывает в цикле в dedicated task'е.
    async fn recv_frame(&self) -> io::Result<(NeighborId, Vec<u8>)>;

    /// Broadcast frame to all reachable neighbors на этом interface'е.
    /// На non-broadcast interfaces — итерирует по known neighbors.
    /// На broadcast (mDNS / LoRa) — одиночный физический broadcast.
    async fn broadcast_frame(&self, frame: &[u8]) -> io::Result<()>;

    /// True if this interface can carry **unreliable datagrams** (QUIC =
    /// yes). Datagrams are loss-tolerant, latency-sensitive — the channel
    /// for realtime media (opus/RTP). The mesh only runs a datagram pump on
    /// interfaces where this is true.
    fn supports_datagrams(&self) -> bool {
        false
    }

    /// Send an unreliable datagram to a neighbor. Unlike `send_frame`, no
    /// retransmit / ordering — dropped under loss. Used for realtime media.
    /// Default: `Unsupported` (only datagram-capable interfaces override).
    async fn send_datagram(&self, neighbor: &NeighborId, payload: &[u8]) -> io::Result<()> {
        let _ = (neighbor, payload);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "interface does not support datagrams",
        ))
    }

    /// Receive the next inbound datagram from any neighbor. Only called on
    /// interfaces where `supports_datagrams()` is true; the default exists
    /// solely to keep the trait object-safe for connectionless interfaces.
    async fn recv_datagram(&self) -> io::Result<(NeighborId, Vec<u8>)> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "interface does not support datagrams",
        ))
    }

    /// True if this interface can carry [`BulkStream`]s. QUIC = yes (a dedicated stream on the
    /// peer's EXISTING connection — no second dial, no extra handshake). The mesh only offers the
    /// bulk plane on interfaces where this holds.
    fn supports_bulk_streams(&self) -> bool {
        false
    }

    /// Open a bulk stream to `neighbor` over the existing connection.
    /// Default: `Unsupported` (frame/datagram-only interfaces).
    async fn open_bulk_stream(&self, neighbor: &NeighborId) -> io::Result<BulkStream> {
        let _ = neighbor;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "interface does not support bulk streams",
        ))
    }

    /// Yield the next inbound bulk stream. Blocking, called in a loop by a dedicated task
    /// (mirrors `recv_frame` / `recv_closed`). Default: never resolves, so an interface without
    /// the bulk plane simply never yields one.
    async fn accept_bulk_stream(&self) -> io::Result<(NeighborId, BulkStream)> {
        std::future::pending().await
    }

    /// Yield the next neighbour whose connection has closed, so the mesh can
    /// prune that path from its routing/PathSet state proactively — not only
    /// lazily on the next failed send. Blocking, called in a loop by a dedicated
    /// per-interface task (mirrors `recv_frame`). Default: never resolves —
    /// interfaces without close detection (connectionless / broadcast) rely on
    /// send errors instead. Connection-based interfaces (QUIC) override.
    /// P2 PeerLink cleanup-on-close (mesh-peer-link-multipath.md).
    async fn recv_closed(&self) -> io::Result<NeighborId> {
        std::future::pending().await
    }

    /// Close every connection this interface holds, telling each peer we are gone, and give the
    /// close a brief moment to reach the wire. Called once when the node stops. Without it a peer
    /// learns of our departure only from its idle timeout or liveness prune (~20 s), and until then
    /// keeps sending into the dead connection — frames it reports as sent and that nobody reads
    /// (docs/bugs/receipt-after-restart-fails-to-decrypt.md). Default: nothing to close
    /// (connectionless interfaces).
    async fn close(&self) {}

    /// Close the one connection `neighbor` names, telling the peer — a direct link the node will not
    /// keep (`Network::keep_direct_only_with`). Default: nothing to close (connectionless interfaces).
    async fn close_neighbor(&self, _neighbor: &NeighborId) {}

    /// Our reflexive address as seen by public STUN servers, asked FROM THIS INTERFACE'S OWN SOCKET —
    /// so the mapping is the one a peer's dial reaches. Default: `None` (only the native-UDP QUIC
    /// carrier has a socket of its own to ask from). See `crate::stun_socket`.
    async fn stun_reflexive(
        &self,
        servers: &[&str],
        timeout: Duration,
    ) -> Option<omni_net_primitives::stun::Reflexive> {
        let _ = (servers, timeout);
        None
    }

    /// The remote socket address we observe for a connected `neighbor`, if this
    /// interface is address-based (QUIC). Used for reflexive-address discovery:
    /// the connection acceptor reports back the source address it saw, so a
    /// peer behind NAT can learn its external mapping without STUN. Default:
    /// `None` (address-less / connectionless interfaces). P1 / review §4.1A.
    fn remote_addr(&self, neighbor: &NeighborId) -> Option<std::net::SocketAddr> {
        let _ = neighbor;
        None
    }

    /// Total bytes received on `neighbor`'s connection (across ALL QUIC streams + datagrams + acks),
    /// if this interface is connection-based (QUIC). Lets the mesh treat connection-level activity —
    /// e.g. an inbound bulk attachment stream, which never reaches the mesh-frame path that refreshes
    /// `last_seen` — as liveness, so an actively-receiving connection is not pruned for mesh-frame
    /// idle alone. Default: `None`. See docs/bugs/chat-gated-on-stale-has-link.md.
    fn connection_rx_bytes(&self, neighbor: &NeighborId) -> Option<u64> {
        let _ = neighbor;
        None
    }

    /// Проход туда-обратно, ИЗМЕРЕННЫЙ транспортом на соединении с `neighbor` — сглаженный RTT
    /// QUIC (RFC 9002), тот же, что уходит в трассу `quic/stats`. Только для connection-based
    /// интерфейсов; по умолчанию `None`.
    ///
    /// Нужен затем, чтобы наверх шло ИЗМЕРЕНИЕ, а не самоделка. Панель связи показывала «Пинг»,
    /// посчитанный по presence-обмену: момент отправки вытаскивался из строки идентификатора
    /// пинга, а между этим моментом и уходом пакета лежат тик очереди, шифрование и два замка.
    /// Живой замер 2026-08-31: presence давал 322 мс (медиана, n=306), транспорт в это же время —
    /// 49 мс, а собеседник отвечал за 11 мс. См. docs/bugs/ping-was-our-own-pipeline.md.
    fn connection_rtt(&self, neighbor: &NeighborId) -> Option<Duration> {
        let _ = neighbor;
        None
    }

    /// Применить смену контроллера перегрузки к ЖИВОМУ интерфейсу: пересобрать конфиги и закрыть
    /// соединения, которые едут на прежнем (quinn не умеет менять контроллер на живом соединении).
    /// Возвращает число закрытых соединений. По умолчанию — нечего делать.
    ///
    /// Существует затем, чтобы оболочке не приходилось ради настройки перезапускать меш: тот сносит
    /// дискавери, анонсы и проброс порта и стоит больше минуты без связи.
    fn refresh_congestion_control(&self) -> std::io::Result<usize> {
        Ok(0)
    }

    /// Route `peer_quic`'s single quinn connection over its direct-TCP door at `door_tcp` — the
    /// connection MIGRATES onto the door path (same connection id), no re-dial. `peer_quic` is the
    /// address quinn dialed the peer at. Only the native-UDP QUIC carrier supports this (it rides a
    /// `MuxRouter`); the default errors. See `mux_socket` + tcp-direct-peer-fallback.md.
    async fn route_peer_over_door(
        &self,
        peer_quic: std::net::SocketAddr,
        door_tcp: std::net::SocketAddr,
    ) -> io::Result<()> {
        let _ = (peer_quic, door_tcp);
        Err(io::Error::other("interface does not support door routing"))
    }

    /// Route `peer_quic` back over the UDP base (the optimistic upgrade); the connection migrates
    /// back and its door tunnel is torn down. Default: no-op.
    fn route_peer_over_udp(&self, peer_quic: std::net::SocketAddr) {
        let _ = peer_quic;
    }

    /// Whether `peer_quic` is currently routed over its door on this interface. Default: `false`.
    fn peer_on_door(&self, peer_quic: std::net::SocketAddr) -> bool {
        let _ = peer_quic;
        false
    }

    /// The door `peer_quic` is currently routed over on this interface, if any. Default: `None`.
    fn peer_door_of(&self, peer_quic: std::net::SocketAddr) -> Option<std::net::SocketAddr> {
        let _ = peer_quic;
        None
    }
}

/// Type-erased handle for interface storage.
pub type InterfaceHandle = Arc<dyn Interface>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::NodeAddress;
    use async_trait::async_trait;
    use parking_lot::Mutex;
    use std::collections::VecDeque;

    /// Mock interface для unit-тестов routing'а (без реального network'а).
    /// H1 (positive): доказательство что Interface trait clean'ный —
    /// мы можем тривиально написать mock реализацию.
    #[derive(Debug)]
    struct MockInterface {
        id: InterfaceId,
        incoming: Mutex<VecDeque<(NeighborId, Vec<u8>)>>,
        outgoing: Mutex<Vec<(Option<NeighborId>, Vec<u8>)>>,
    }

    impl MockInterface {
        fn new(id: &str) -> Self {
            Self {
                id: InterfaceId(id.to_string()),
                incoming: Mutex::new(VecDeque::new()),
                outgoing: Mutex::new(Vec::new()),
            }
        }

        fn inject_incoming(&self, neighbor: &str, payload: Vec<u8>) {
            self.incoming
                .lock()
                .push_back((NeighborId(neighbor.to_string()), payload));
        }

        fn taken_outgoing(&self) -> Vec<(Option<NeighborId>, Vec<u8>)> {
            std::mem::take(&mut *self.outgoing.lock())
        }
    }

    #[async_trait]
    impl Interface for MockInterface {
        fn id(&self) -> InterfaceId {
            self.id.clone()
        }

        fn kind(&self) -> InterfaceKind {
            InterfaceKind::QuicOverUdp
        }

        fn mtu(&self) -> usize {
            1200
        }

        fn bandwidth_estimate(&self) -> u64 {
            1_000_000_000
        }

        fn latency_estimate(&self) -> Duration {
            Duration::from_millis(1)
        }

        fn reliable(&self) -> bool {
            true
        }

        fn broadcast_capable(&self) -> bool {
            false
        }

        async fn send_frame(&self, neighbor: &NeighborId, frame: &[u8]) -> io::Result<()> {
            self.outgoing
                .lock()
                .push((Some(neighbor.clone()), frame.to_vec()));
            Ok(())
        }

        async fn recv_frame(&self) -> io::Result<(NeighborId, Vec<u8>)> {
            // Drain one item; если очередь пуста — block forever.
            // Для тестов мы всегда инжектим заранее.
            loop {
                if let Some(item) = self.incoming.lock().pop_front() {
                    return Ok(item);
                }
                tokio::task::yield_now().await;
            }
        }

        async fn broadcast_frame(&self, frame: &[u8]) -> io::Result<()> {
            self.outgoing.lock().push((None, frame.to_vec()));
            Ok(())
        }
    }

    /// Используем NodeAddress в тестах чтобы тип не висел unused.
    fn _addr_check(addr: &NodeAddress) -> usize {
        addr.as_bytes().len()
    }

    #[tokio::test]
    async fn mock_interface_round_trip_send_recv() {
        let iface = MockInterface::new("mock-0");
        iface.inject_incoming("peer-A", vec![1, 2, 3]);

        let (neighbor, frame) = iface.recv_frame().await.unwrap();
        assert_eq!(neighbor.0, "peer-A");
        assert_eq!(frame, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn mock_interface_send_records_destination() {
        let iface = MockInterface::new("mock-1");
        iface
            .send_frame(&NeighborId("peer-B".to_string()), &[7, 8, 9])
            .await
            .unwrap();

        let outgoing = iface.taken_outgoing();
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0].0, Some(NeighborId("peer-B".to_string())));
        assert_eq!(outgoing[0].1, vec![7, 8, 9]);
    }

    #[tokio::test]
    async fn mock_interface_broadcast_records_no_destination() {
        let iface = MockInterface::new("mock-2");
        iface.broadcast_frame(&[10, 20]).await.unwrap();

        let outgoing = iface.taken_outgoing();
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0].0, None);
        assert_eq!(outgoing[0].1, vec![10, 20]);
    }

    #[test]
    fn interface_trait_object_safe() {
        // H1: trait object-safe — можем хранить как Arc<dyn Interface>.
        let iface: InterfaceHandle = Arc::new(MockInterface::new("mock-3"));
        assert_eq!(iface.id().0, "mock-3");
        assert_eq!(iface.kind(), InterfaceKind::QuicOverUdp);
        assert_eq!(iface.mtu(), 1200);
    }
}
