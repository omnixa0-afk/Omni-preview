//! `MuxRouter` — a [`quinn::AsyncUdpSocket`] that carries ONE quinn endpoint's connections over a
//! UDP base **plus** per-peer TCP-door tunnels, switchable at runtime BY DESTINATION. Switching a
//! peer to/from its door keeps that peer's quinn CONNECTION alive: quinn keeps sending to the same
//! peer address, the bytes just ride a different transport, so the far end sees the same connection
//! id arrive from a new source and MIGRATES the path (server migration is on by default). No fresh
//! connection, no ephemeral-peer churn, no door idle-death, no door-owner prune of a bridge peer —
//! the whole class collapses into a standard QUIC path migration (Meta-B).
//!
//! This is the transport primitive under the reliable-base + optimistic-upgrade carrier policy: the
//! policy calls [`MuxRouter::route_over_door`] to fail a peer down onto its reliable direct-TCP door
//! and [`MuxRouter::route_over_udp`] to promote it back to native UDP once UDP proves stable.
//! See docs/bugs/relay-less-mobile-transport-findings.md §THE REMAINING BLOCKER.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use parking_lot::Mutex;
use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};

use crate::tcp_transport::TcpQuicSocket;

/// The key of a peer in [`MuxRouter::doors`]: its address with a v4-mapped IPv6 (`[::ffff:a.b.c.d]`)
/// read as the IPv4 it is. On a dual-stack socket quinn addresses an IPv4 peer v4-mapped, while the
/// door route is registered with the plain IPv4 — keyed apart, the first contact's Initial missed
/// its tunnel, went out raw to the door's TCP port and was lost (live 2026-10-16: a laptop that
/// joined a Wi-Fi with IPv6 never came up over a door that answered TLS each time).
pub(crate) fn peer_key(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}

/// A per-peer door tunnel currently carrying that peer's packets (a `TcpQuicSocket` to the peer's
/// front-door bridge). Present in [`MuxRouter::doors`] iff the peer is routed over its door.
#[derive(Debug)]
struct DoorTunnel {
    tcp: Arc<TcpQuicSocket>,
    /// The door endpoint this tunnel dials. Compared on re-route so a CHANGED door (peer restart /
    /// UPnP port rotation / a stale cross-session entry) replaces the tunnel instead of being skipped
    /// by idempotency — the tunnel would otherwise keep dialing a dead port and deliver nothing.
    door: SocketAddr,
}

/// See the module docs. One per client endpoint; wraps the endpoint's UDP base socket and holds a
/// per-destination map of active door tunnels.
#[derive(Debug)]
pub struct MuxRouter {
    /// The UDP base. Every peer not currently routed over a door rides this.
    udp: Arc<dyn AsyncUdpSocket>,
    /// Peer's quic address (how quinn addresses it) → its active door tunnel. Absent ⇒ on UDP.
    doors: Mutex<HashMap<SocketAddr, DoorTunnel>>,
    local: SocketAddr,
    /// Who is waiting in `poll_recv`. A tunnel added after that poll never got its waker, so the
    /// peer's answers would sit in it unread until something else woke the endpoint: adding one wakes
    /// the waiter to poll it too.
    recv_waker: Mutex<Option<std::task::Waker>>,
}

impl MuxRouter {
    /// Wrap a UDP base socket (already `quinn::Runtime::wrap_udp_socket`-ed, possibly STUN-wrapped).
    pub fn new(udp: Arc<dyn AsyncUdpSocket>) -> io::Result<Arc<Self>> {
        let local = udp.local_addr()?;
        Ok(Arc::new(Self {
            udp,
            doors: Mutex::new(HashMap::new()),
            local,
            recv_waker: Mutex::new(None),
        }))
    }

    /// Route `peer_quic`'s packets over its front door at `door_tcp`, connecting a tunnel if there
    /// isn't one. quinn's existing connection to `peer_quic` migrates onto the door path. Idempotent
    /// for the SAME door (a peer already on this exact door is left as-is); a DIFFERENT `door_tcp`
    /// REPLACES the tunnel (the old one, dialing a now-dead port after a peer restart / UPnP rotation,
    /// would otherwise be kept forever by a bare presence check and deliver nothing). `sni` is the
    /// tunnel's outer-TLS server name.
    pub async fn route_over_door(
        &self,
        peer_quic: SocketAddr,
        door_tcp: SocketAddr,
        sni: &str,
    ) -> io::Result<()> {
        // «Уже на этой двери» — только если труба ЖИВА. Супервизор туннеля в дверь сдаётся, когда
        // адрес исчез (у пира за NAT порт меняется при перезапуске), и без проверки живости запись
        // о сдавшемся туннеле выглядела бы исправной: no-op на трупе, и пир недостижим до
        // перезапуска процесса.
        if self
            .doors
            .lock()
            .get(&peer_key(peer_quic))
            .is_some_and(|t| t.door == door_tcp && t.tcp.is_alive())
        {
            return Ok(()); // already on this exact door — genuine no-op
        }
        // No tunnel, or one pointing at a stale/changed door → (re)connect. Dropping the old
        // `DoorTunnel` closes its `TcpQuicSocket` (its supervisor sees the send half go away and exits).
        //
        // ДВЕРЬ ЭФЕМЕРНА, поэтому туннель в неё имеет право сдаться. Карта дверей ключуется UDP-
        // адресом пира, а он меняется вместе с портом при каждом перезапуске: старая запись новым
        // ключом не вытесняется и живёт до конца процесса. Пока супервизор был вечным, это стоило
        // 6189 неудачных дозвонов в два мёртвых адреса за одну сессию (замер 2026-08-30). Отказ
        // безопасен: `route_over_door` зовётся по событию и поднимет туннель заново.
        //
        // Подключение — БЕЗ карты дверей в руках: оно ждёт до своего таймаута, а карту берёт каждый
        // исходящий пакет сокета (`try_send`), чтобы выбрать маршрут. Под ней дозвон в мёртвую дверь
        // останавливал отправку ВСЕМ пирам узла: живьём 2026-10-10 — ~8 с каждый обход рандеву.
        let tcp = TcpQuicSocket::connect_with_policy(
            door_tcp,
            sni,
            crate::tcp_transport::TcpReconnectPolicy::GiveUpAfter(
                crate::tcp_transport::DOOR_GIVE_UP_AFTER_FAILURES,
            ),
        )
        .await?;
        self.doors.lock().insert(peer_key(peer_quic), DoorTunnel { tcp, door: door_tcp });
        if let Some(waker) = self.recv_waker.lock().take() {
            waker.wake();
        }
        Ok(())
    }

    /// Route `peer_quic` back over the UDP base and tear its door tunnel down (the optimistic
    /// upgrade). quinn's connection migrates back to the UDP path. Idempotent.
    pub fn route_over_udp(&self, peer_quic: SocketAddr) {
        self.doors.lock().remove(&peer_key(peer_quic));
    }

    /// Whether `peer_quic` is currently routed over its door (vs the UDP base).
    pub fn is_on_door(&self, peer_quic: SocketAddr) -> bool {
        self.doors.lock().contains_key(&peer_key(peer_quic))
    }

    /// The door `peer_quic` is routed over — what a failed migrate-back returns it to.
    pub fn door_of(&self, peer_quic: SocketAddr) -> Option<SocketAddr> {
        self.doors.lock().get(&peer_key(peer_quic)).map(|t| t.door)
    }
}

impl AsyncUdpSocket for MuxRouter {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        // Writability is the UDP base's — door tunnels are unbounded (their `try_send` never blocks).
        Arc::clone(&self.udp).create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        // Route by destination: a peer with an active door tunnel rides it; everyone else the base.
        let door = self
            .doors
            .lock()
            .get(&peer_key(transmit.destination))
            .map(|d| Arc::clone(&d.tcp));
        match door {
            Some(tcp) => send_split_over_door(&tcp, transmit),
            None => self.udp.try_send(transmit),
        }
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [std::io::IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        // Poll the UDP base first, then every door tunnel. Every polled source registers its waker,
        // so quinn is re-polled when data lands on ANY of them. Returns the first source with data.
        {
            let mut waiting = self.recv_waker.lock();
            if !waiting.as_ref().is_some_and(|known| known.will_wake(cx.waker())) {
                *waiting = Some(cx.waker().clone());
            }
        }
        if let Poll::Ready(Ok(n)) = self.udp.poll_recv(cx, bufs, meta) {
            if n > 0 {
                return Poll::Ready(Ok(n));
            }
        }
        // Snapshot the tunnels so the lock isn't held across the (non-blocking) polls.
        let tunnels: Vec<Arc<TcpQuicSocket>> =
            self.doors.lock().values().map(|d| Arc::clone(&d.tcp)).collect();
        for tcp in tunnels {
            if let Poll::Ready(Ok(n)) = tcp.poll_recv(cx, bufs, meta) {
                if n > 0 {
                    return Poll::Ready(Ok(n));
                }
            }
        }
        Poll::Pending
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local)
    }

    // Segmentation offload (GSO/GRO) capability is the BASE socket's, not ours. Hardcoding 1 here
    // told quinn "one datagram per syscall" and threw away the base's batching — measured on a real
    // LAN, the sender burned ~94% of one core to push only ~23.6k packets/s (~40 us of CPU per
    // packet), pinning an attachment transfer at ~265 Mbit/s while plain scp did ~695 Mbit/s over the
    // same link. Neither congestion control (cwnd 162 MB, loss 0) nor flow-control windows were
    // limiting: the cost was one send syscall per packet. Delegating restores batching on the UDP
    // base, and naturally still reports 1 when the base is the STUN-wrapped socket (which genuinely
    // is per-datagram). Door tunnels are handled in `try_send`, which splits a batch back apart.
    fn max_transmit_segments(&self) -> usize {
        self.udp.max_transmit_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.udp.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        false
    }
}

/// Send one transmit over a peer's door tunnel.
///
/// A `TcpQuicSocket` carries exactly ONE datagram per `try_send` (it frames `contents` as a single
/// datagram over the TCP conn), so a GSO batch aimed at a peer that is currently on its door has to
/// be split back into individual datagrams first — otherwise the whole batch would arrive as one
/// oversized datagram and be rejected. Peers on the UDP base keep the batch intact.
fn send_split_over_door(tcp: &TcpQuicSocket, transmit: &Transmit) -> io::Result<()> {
    let segment_size = match transmit.segment_size {
        Some(segment_size) if segment_size > 0 && segment_size < transmit.contents.len() => {
            segment_size
        }
        // Not a batch (or a single full-size segment): pass through untouched.
        _ => return tcp.try_send(transmit),
    };
    for chunk in transmit.contents.chunks(segment_size) {
        tcp.try_send(&Transmit {
            destination: transmit.destination,
            ecn: transmit.ecn,
            contents: chunk,
            segment_size: None,
            src_ip: transmit.src_ip,
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cert::derive_cert_from_signing_key;
    use crate::tcp_transport::run_tcp_bridge_on;
    use ed25519_dalek::SigningKey;
    use omni_net_primitives::{bind_quic_udp_socket, omni_client_config, omni_server_config};
    use quinn::{Endpoint, EndpointConfig, Runtime, TokioRuntime};
    use rand_core::OsRng;
    use std::time::Duration;
    use tokio::net::TcpListener;

    /// One bidi request/echo round-trip — proves BOTH directions carry on the peer's current transport.
    async fn round_trip(conn: &quinn::Connection, msg: &[u8]) -> Vec<u8> {
        let (mut send, mut recv) = conn.open_bi().await.expect("open_bi");
        send.write_all(msg).await.expect("write");
        send.finish().expect("finish");
        recv.read_to_end(64).await.expect("read echo")
    }

    /// Spawn a quinn echo server on a fresh UDP socket; returns (endpoint, its quic addr). Accepts one
    /// connection and echoes bidi streams forever (held alive by the returned task via the Endpoint).
    async fn echo_server(alpn: &[u8], runtime: Arc<TokioRuntime>) -> (Endpoint, SocketAddr) {
        let skey = SigningKey::generate(&mut OsRng);
        let (cert, key) = derive_cert_from_signing_key(&skey).unwrap();
        let cfg = omni_server_config(cert, key, alpn).unwrap();
        let udp = bind_quic_udp_socket("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = udp.local_addr().unwrap();
        let socket = runtime.wrap_udp_socket(udp).unwrap();
        let ep = Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            Some(cfg),
            socket,
            runtime,
        )
        .unwrap();
        let ep2 = ep.clone();
        tokio::spawn(async move {
            while let Some(incoming) = ep2.accept().await {
                tokio::spawn(async move {
                    let Ok(conn) = incoming.await else { return };
                    while let Ok((mut s, mut r)) = conn.accept_bi().await {
                        let req = match r.read_to_end(64).await {
                            Ok(b) => b,
                            Err(_) => break,
                        };
                        if s.write_all(&req).await.is_err() {
                            break;
                        }
                        let _ = s.finish();
                    }
                });
            }
        });
        (ep, addr)
    }

    /// The `MuxRouter` routes ONE quinn endpoint's connections BY DESTINATION: peer B rides its
    /// direct-TCP door while peer C stays on native UDP, and B's connection MIGRATES door↔UDP without
    /// dropping — all on one endpoint. Proves per-destination routing, migration both ways, and that
    /// switching one peer never disturbs another.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mux_router_routes_by_destination_and_migrates_one_peer() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let alpn = b"omni-mesh-mux-test";
        let runtime = Arc::new(TokioRuntime);

        // B — echo server with a TCP door bridge in front of its quinn.
        let (b_ep, b_quic) = echo_server(alpn, runtime.clone()).await;
        let _ = &b_ep;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let b_door = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = run_tcp_bridge_on(listener, b_quic).await;
        });

        // C — echo server on UDP only (stays UDP the whole test; the isolation control).
        let (c_ep, c_quic) = echo_server(alpn, runtime.clone()).await;
        let _ = &c_ep;

        // A — ONE client endpoint over a MuxRouter over a real UDP base.
        let a_udp = bind_quic_udp_socket("127.0.0.1:0".parse().unwrap()).unwrap();
        let a_base: Arc<dyn AsyncUdpSocket> = runtime.wrap_udp_socket(a_udp).unwrap();
        let mux = MuxRouter::new(a_base).unwrap();
        let mut client = Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            None,
            Arc::clone(&mux) as Arc<dyn AsyncUdpSocket>,
            runtime.clone(),
        )
        .unwrap();
        client.set_default_client_config(omni_client_config(alpn).unwrap());

        // Dial both peers — over UDP (no doors yet).
        let conn_b = tokio::time::timeout(
            Duration::from_secs(10),
            client.connect(b_quic, "localhost").unwrap(),
        )
        .await
        .expect("B handshake")
        .expect("B connected");
        let conn_c = tokio::time::timeout(
            Duration::from_secs(10),
            client.connect(c_quic, "localhost").unwrap(),
        )
        .await
        .expect("C handshake")
        .expect("C connected");

        assert_eq!(round_trip(&conn_b, b"b-udp").await, b"b-udp", "B over UDP");
        assert_eq!(round_trip(&conn_c, b"c-udp").await, b"c-udp", "C over UDP");

        // DOWNGRADE B onto its door — C is untouched.
        mux.route_over_door(b_quic, b_door, "localhost").await.unwrap();
        assert!(mux.is_on_door(b_quic) && !mux.is_on_door(c_quic));
        assert_eq!(
            round_trip(&conn_b, b"b-door").await,
            b"b-door",
            "B migrated onto the direct-TCP door (same connection)"
        );
        assert_eq!(
            round_trip(&conn_c, b"c-still-udp").await,
            b"c-still-udp",
            "C stays on UDP while B is on the door (per-destination routing)"
        );

        // CHANGED door (peer restart / UPnP port rotation): re-routing to a DIFFERENT door must
        // REPLACE the tunnel, not be skipped by a bare presence check — the stale tunnel would keep
        // dialing a dead port and deliver nothing. Same b_quic behind a fresh bridge port.
        let listener2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let b_door2 = listener2.local_addr().unwrap();
        assert_ne!(b_door, b_door2);
        tokio::spawn(async move {
            let _ = run_tcp_bridge_on(listener2, b_quic).await;
        });
        mux.route_over_door(b_quic, b_door2, "localhost").await.unwrap();
        assert!(mux.is_on_door(b_quic));
        assert_eq!(
            round_trip(&conn_b, b"b-door2").await,
            b"b-door2",
            "B migrated onto a CHANGED door (tunnel replaced, not skipped by idempotency)"
        );

        // UPGRADE B back to UDP — connection migrates back.
        mux.route_over_udp(b_quic);
        assert!(!mux.is_on_door(b_quic));
        assert_eq!(
            round_trip(&conn_b, b"b-back-udp").await,
            b"b-back-udp",
            "B migrated back onto UDP (same connection)"
        );

        conn_b.close(0u32.into(), b"done");
        conn_c.close(0u32.into(), b"done");
    }

    /// First contact over a door from a DUAL-STACK socket: quinn addresses the IPv4 peer v4-mapped
    /// (`[::ffff:127.0.0.1]`), the route was registered with the plain IPv4 — it must still be the
    /// door's. The peer's "quic address" here answers nothing over UDP, so only the door can carry
    /// the handshake (live 2026-10-16: a laptop on a Wi-Fi with IPv6 never came up over a door).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dual_stack_socket_reaches_an_ipv4_peer_through_its_door() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let alpn = b"omni-mesh-mux-test";
        let runtime = Arc::new(TokioRuntime);
        let (b_ep, b_quic) = echo_server(alpn, runtime.clone()).await;
        let _ = &b_ep;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let b_door = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = run_tcp_bridge_on(listener, b_quic).await;
        });
        // Where A believes B is: nobody listens there — UDP to it goes nowhere.
        let nowhere = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let b_seen = nowhere.local_addr().unwrap();
        drop(nowhere);

        let a_udp = bind_quic_udp_socket("[::]:0".parse().unwrap()).unwrap();
        let a_base: Arc<dyn AsyncUdpSocket> = runtime.wrap_udp_socket(a_udp).unwrap();
        let mux = MuxRouter::new(a_base).unwrap();
        let mut client = Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            None,
            Arc::clone(&mux) as Arc<dyn AsyncUdpSocket>,
            runtime.clone(),
        )
        .unwrap();
        client.set_default_client_config(omni_client_config(alpn).unwrap());

        mux.route_over_door(b_seen, b_door, "localhost").await.unwrap();
        let mapped = SocketAddr::new(std::net::IpAddr::V6(match b_seen.ip() {
            std::net::IpAddr::V4(v4) => v4.to_ipv6_mapped(),
            std::net::IpAddr::V6(v6) => v6,
        }), b_seen.port());
        assert!(mux.is_on_door(mapped), "the v4-mapped address is the same peer");
        let conn = tokio::time::timeout(Duration::from_secs(10), client.connect(b_seen, "localhost").unwrap())
            .await
            .expect("handshake over the door")
            .expect("connected");
        assert_eq!(round_trip(&conn, b"over-the-door").await, b"over-the-door");
        conn.close(0u32.into(), b"done");
    }

    /// A door that does not answer must not stop the node's other traffic. The TCP connect to it
    /// waits up to its timeout; it used to wait holding the door map — which every outgoing QUIC
    /// packet of the socket locks to pick its route — and blocking the runtime thread it ran on. Live
    /// 2026-10-10: a dial to a dead door froze the laptop's sends ~8 s each sweep
    /// (`mesh/tick-overrun contacts=8722ms`). One runtime thread here, so a blocking connect would
    /// stall the rest of this test too.
    #[tokio::test(flavor = "current_thread")]
    async fn a_door_that_does_not_answer_does_not_hold_up_other_peers() {
        let runtime = Arc::new(TokioRuntime);
        let a_udp = bind_quic_udp_socket("127.0.0.1:0".parse().unwrap()).unwrap();
        let a_base: Arc<dyn AsyncUdpSocket> = runtime.wrap_udp_socket(a_udp).unwrap();
        let mux = MuxRouter::new(a_base).unwrap();
        // Nothing answers there: the connect hangs until its timeout (a black-holed address).
        let dead_door: SocketAddr = "10.255.255.1:9".parse().unwrap();
        let routing = Arc::clone(&mux);
        let started = std::time::Instant::now();
        let dial = tokio::spawn(async move {
            let _ = routing
                .route_over_door("127.0.0.1:1".parse().unwrap(), dead_door, "localhost")
                .await;
            started.elapsed()
        });
        let napped = std::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let nap = napped.elapsed();
        let other: SocketAddr = "127.0.0.1:2".parse().unwrap();
        let looked_at = std::time::Instant::now();
        assert!(!mux.is_on_door(other));
        let waited = looked_at.elapsed();
        let dial_took = dial.await.unwrap();
        if dial_took < Duration::from_secs(1) {
            eprintln!("SKIP: this host refuses the black-holed address at once ({dial_took:?})");
            return;
        }
        assert!(
            nap < Duration::from_secs(1),
            "the runtime thread was held {nap:?} by a door connect of {dial_took:?}"
        );
        assert!(
            waited < Duration::from_millis(200),
            "another peer's route waited {waited:?} behind a door connect of {dial_took:?}"
        );
    }
}
