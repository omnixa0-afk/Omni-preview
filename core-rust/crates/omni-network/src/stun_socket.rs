//! `StunSocket` — STUN on the mesh's own QUIC socket.
//!
//! The only reflexive address worth advertising is the NAT mapping of the socket that carries our
//! QUIC: that is the port a peer's dial actually lands on. The rendezvous helper used to ask STUN
//! from a throwaway socket of its own, closed right after the query, so the `ip:port` it put into
//! the beacon led nowhere — live 2026-09-24 the PC dialed the laptop at exactly that dead port.
//! This is how srflx candidates are gathered everywhere else too (ICE / libjuice / pion / iroh):
//! the binding request leaves from the socket that will carry the traffic.
//!
//! The wrapper sits directly on the raw UDP socket — BELOW the QUIC endpoint (STUN
//! servers speak plain STUN) and below the `MuxRouter`. It sends binding requests itself and, on
//! receive, takes out exactly the datagrams that answer a request it is waiting for (matching txid
//! AND the server that was asked); everything else — all of QUIC — passes through untouched.
//! See docs/bugs/every-nat-was-classified-symmetric.md.

use std::collections::HashMap;
use std::io::{self, IoSliceMut};
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use omni_net_primitives::stun::{
    build_binding_request, first_ipv4, fresh_txid, parse_mapped_address, reflexive_consensus,
    stun_transaction_id,
};
pub use omni_net_primitives::stun::Reflexive;
use parking_lot::Mutex;
use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use tokio::sync::oneshot;

pub use omni_net_primitives::stun::DEFAULT_STUN_SERVERS;

/// A request in flight: who was asked (the answer must come from there) and who is waiting.
#[derive(Debug)]
struct Pending {
    server: SocketAddr,
    reply: oneshot::Sender<SocketAddr>,
}

#[derive(Debug)]
pub struct StunSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    local: SocketAddr,
    pending: Mutex<HashMap<[u8; 12], Pending>>,
}

impl StunSocket {
    pub fn new(inner: Arc<dyn AsyncUdpSocket>) -> io::Result<Arc<Self>> {
        let local = inner.local_addr()?;
        Ok(Arc::new(Self {
            inner,
            local,
            pending: Mutex::new(HashMap::new()),
        }))
    }

    /// Ask every server in `servers` (`host:port`) at once, from this socket, and cross-check the
    /// answers (`reflexive_consensus`). Parallel, so a blocked operator (Google STUN does not answer
    /// from here) costs one `timeout`, not one per server.
    pub async fn reflexive(&self, servers: &[&str], timeout: Duration) -> Option<Reflexive> {
        let mut waiting = Vec::new();
        for server in servers {
            let Ok(resolved) = tokio::net::lookup_host(*server).await else {
                continue;
            };
            let Ok(address) = first_ipv4(server, resolved) else {
                continue;
            };
            if let Ok(reply) = self.send_request(address) {
                waiting.push(reply);
            }
        }
        let deadline = tokio::time::Instant::now() + timeout;
        let mut answers = Vec::new();
        for (txid, reply) in waiting {
            if let Ok(Ok(mapped)) = tokio::time::timeout_at(deadline, reply).await {
                answers.push(mapped);
            }
            self.pending.lock().remove(&txid);
        }
        reflexive_consensus(&answers)
    }

    fn send_request(
        &self,
        server: SocketAddr,
    ) -> io::Result<([u8; 12], oneshot::Receiver<SocketAddr>)> {
        let txid = fresh_txid();
        let (reply, answer) = oneshot::channel();
        self.pending.lock().insert(txid, Pending { server, reply });
        let request = build_binding_request(&txid);
        let sent = self.inner.try_send(&Transmit {
            destination: self.addressable(server),
            ecn: None,
            contents: &request,
            segment_size: None,
            src_ip: None,
        });
        if let Err(error) = sent {
            self.pending.lock().remove(&txid);
            return Err(error);
        }
        Ok((txid, answer))
    }

    /// The mesh socket is dual-stack IPv6 (`[::]:port`), where an IPv4 destination has to be spelt
    /// as v4-mapped — quinn does this for its own connections, we do it for ours.
    fn addressable(&self, server: SocketAddr) -> SocketAddr {
        match (self.local, server) {
            (SocketAddr::V6(_), SocketAddr::V4(v4)) => {
                SocketAddr::new(IpAddr::V6(v4.ip().to_ipv6_mapped()), v4.port())
            }
            _ => server,
        }
    }

    /// If `datagram` answers a request we are waiting for, hand the mapped address over and report
    /// it consumed. Anything else — including a STUN-shaped datagram nobody asked for, or one from
    /// a different address than the server we asked — stays in the stream for QUIC.
    fn take_answer(&self, datagram: &[u8], from: SocketAddr) -> bool {
        let Some(txid) = stun_transaction_id(datagram) else {
            return false;
        };
        let mut pending = self.pending.lock();
        let Some(request) = pending.get(&txid) else {
            return false;
        };
        if canonical(request.server) != canonical(from) {
            return false;
        }
        let Some(mapped) = parse_mapped_address(datagram, &txid) else {
            return false;
        };
        let request = pending.remove(&txid).expect("looked up above");
        let _ = request.reply.send(mapped);
        true
    }

    /// Remove answered STUN datagrams from a receive batch, compacting the rest to the front.
    /// Returns how many entries remain. A GRO-coalesced entry (`stride < len`) holds several
    /// datagrams of one size from one source — never a STUN answer — and is left alone.
    fn filter_batch(&self, bufs: &mut [IoSliceMut<'_>], meta: &mut [RecvMeta], count: usize) -> usize {
        let mut kept = 0;
        for index in 0..count {
            let entry = meta[index];
            let single = entry.stride >= entry.len;
            if single && self.take_answer(&bufs[index][..entry.len], entry.addr) {
                continue;
            }
            if kept != index {
                let (front, back) = bufs.split_at_mut(index);
                front[kept][..entry.len].copy_from_slice(&back[0][..entry.len]);
                meta[kept] = entry;
            }
            kept += 1;
        }
        kept
    }
}

/// `::ffff:a.b.c.d` and `a.b.c.d` are the same peer; the socket reports the former.
fn canonical(address: SocketAddr) -> SocketAddr {
    match address {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), v6.port()),
            None => address,
        },
        v4 => v4,
    }
}

impl AsyncUdpSocket for StunSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Arc::clone(&self.inner).create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        self.inner.try_send(transmit)
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        loop {
            let count = match self.inner.poll_recv(cx, bufs, meta) {
                Poll::Ready(Ok(count)) => count,
                other => return other,
            };
            let kept = self.filter_batch(bufs, meta, count);
            if kept > 0 || count == 0 {
                return Poll::Ready(Ok(kept));
            }
            // The whole batch was STUN. Poll again rather than report Pending: the inner socket was
            // Ready, so it registered no waker, and a Pending here would stall QUIC until some
            // unrelated wake-up.
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local)
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cert::derive_cert_from_signing_key;
    use ed25519_dalek::SigningKey;
    use omni_net_primitives::stun::STUN_MAGIC;
    use omni_net_primitives::{bind_quic_udp_socket, omni_client_config, omni_server_config};
    use quinn::{Endpoint, EndpointConfig, Runtime, TokioRuntime};
    use rand_core::OsRng;

    /// A Binding Success Response carrying XOR-MAPPED-ADDRESS `mapped` (IPv4) for `txid`.
    fn binding_response(txid: &[u8], mapped: SocketAddr) -> Vec<u8> {
        let SocketAddr::V4(v4) = canonical(mapped) else { panic!("IPv4 only") };
        let magic = STUN_MAGIC.to_be_bytes();
        let mut resp = vec![0x01, 0x01, 0x00, 0x0c];
        resp.extend_from_slice(&magic);
        resp.extend_from_slice(txid);
        resp.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x01]);
        resp.extend_from_slice(&(v4.port() ^ (STUN_MAGIC >> 16) as u16).to_be_bytes());
        for (octet, m) in v4.ip().octets().iter().zip(magic) {
            resp.push(octet ^ m);
        }
        resp
    }

    /// A loopback STUN server answering every request with the source it saw. With `spoof`, the
    /// answer leaves from ANOTHER socket — an off-path party guessing at our requests.
    async fn fake_stun_server(spoof: bool) -> String {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let other = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            while let Ok((n, from)) = socket.recv_from(&mut buf).await {
                if n < 20 {
                    continue;
                }
                let answer = binding_response(&buf[8..20], from);
                let replier = if spoof { &other } else { &socket };
                let _ = replier.send_to(&answer, from).await;
            }
        });
        address.to_string()
    }

    /// A dual-stack `[::]` socket, as the mesh binds it, wrapped in `StunSocket`, with a quinn
    /// client endpoint on top — the production stack minus mux.
    fn mesh_like_endpoint() -> (Arc<StunSocket>, Endpoint, u16) {
        let runtime = Arc::new(TokioRuntime);
        let udp = bind_quic_udp_socket("[::]:0".parse().unwrap()).unwrap();
        let port = udp.local_addr().unwrap().port();
        let stun = StunSocket::new(runtime.wrap_udp_socket(udp).unwrap()).unwrap();
        let mut endpoint = Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            None,
            Arc::clone(&stun) as Arc<dyn AsyncUdpSocket>,
            runtime,
        )
        .unwrap();
        endpoint.set_default_client_config(omni_client_config(b"stun-socket-test").unwrap());
        (stun, endpoint, port)
    }

    async fn echo_server() -> SocketAddr {
        let key = SigningKey::generate(&mut OsRng);
        let (cert, pk) = derive_cert_from_signing_key(&key).unwrap();
        let config = omni_server_config(cert, pk, b"stun-socket-test").unwrap();
        let endpoint = Endpoint::server(config, "127.0.0.1:0".parse().unwrap()).unwrap();
        let address = endpoint.local_addr().unwrap();
        tokio::spawn(async move {
            while let Some(incoming) = endpoint.accept().await {
                tokio::spawn(async move {
                    let Ok(conn) = incoming.await else { return };
                    while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                        let Ok(request) = recv.read_to_end(64).await else { break };
                        let _ = send.write_all(&request).await;
                        let _ = send.finish();
                    }
                });
            }
        });
        address
    }

    async fn round_trip(conn: &quinn::Connection, message: &[u8]) -> Vec<u8> {
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        send.write_all(message).await.unwrap();
        send.finish().unwrap();
        recv.read_to_end(64).await.unwrap()
    }

    /// The point of the whole module: the address STUN reports is the port of the socket that
    /// carries QUIC — what a peer's dial reaches — and QUIC on that socket is untouched by it, both
    /// before and during a live connection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stun_reports_the_quic_sockets_own_port_and_quic_keeps_flowing() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (stun, endpoint, port) = mesh_like_endpoint();
        let servers = [fake_stun_server(false).await, fake_stun_server(false).await];
        let servers: Vec<&str> = servers.iter().map(String::as_str).collect();

        let before = stun.reflexive(&servers, Duration::from_secs(3)).await.expect("consensus");
        assert_eq!(before.addr, SocketAddr::from(([127, 0, 0, 1], port)), "the QUIC socket's port");
        assert!(before.endpoint_independent, "one socket, one mapping");

        let server = echo_server().await;
        let conn = endpoint.connect(server, "localhost").unwrap().await.expect("QUIC handshake");
        assert_eq!(round_trip(&conn, b"before").await, b"before");

        let during = stun.reflexive(&servers, Duration::from_secs(3)).await.expect("consensus");
        assert_eq!(during, before, "same mapping while QUIC is live on the socket");
        assert_eq!(round_trip(&conn, b"after").await, b"after", "STUN did not eat QUIC");
        conn.close(0u32.into(), b"done");
    }

    /// Negative half: an answer with the right txid from a source we did not ask is not taken — so
    /// the positive test above can't pass by accepting anything STUN-shaped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_answer_from_a_source_that_was_not_asked_is_not_taken() {
        let (stun, _endpoint, _port) = mesh_like_endpoint();
        let servers = [fake_stun_server(true).await, fake_stun_server(true).await];
        let servers: Vec<&str> = servers.iter().map(String::as_str).collect();
        assert_eq!(stun.reflexive(&servers, Duration::from_millis(500)).await, None);
        assert!(stun.pending.lock().is_empty(), "timed-out requests are forgotten");
    }

    /// Live, against the public servers from the mesh-like dual-stack socket. Needs internet.
    /// Run: `cargo test -p omni-network stun_live -- --ignored --nocapture`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore]
    async fn stun_live_public_servers_answer_on_the_mesh_socket() {
        let (stun, _endpoint, port) = mesh_like_endpoint();
        let reflexive = stun.reflexive(DEFAULT_STUN_SERVERS, Duration::from_secs(3)).await;
        println!("local port {port}, reflexive (consensus) = {reflexive:?}");
        assert!(reflexive.is_some(), "≥2 public STUN servers should agree");
    }
}