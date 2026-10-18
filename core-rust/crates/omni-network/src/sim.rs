//! Deterministic in-process network simulator (test harness).
//!
//! A [`SimFabric`] is a shared virtual network; [`SimInterface`] implements
//! [`crate::interface::Interface`] over it, so real [`crate::network::Network`]
//! instances can be wired together without sockets. This is the gate harness the
//! external review calls for: multi-node scenarios (NAT classes, relay, partition,
//! loss, timing) driven deterministically. The frame plane is modelled; the
//! unreliable datagram (media) plane is not (mesh chat/control ride frames).
//!
//! NAT model: each node registers with a [`NatModel`]. On every outbound frame
//! the fabric computes the source address the *destination* sees (the node's
//! external mapping) and records a binding so replies to that mapped address
//! route back — exactly what reflexive discovery (P1) and hole-punching (P2) need
//! to reason about. `NatModel::Cone` yields one stable external address for all
//! destinations (endpoint-independent); `Symmetric` yields a distinct external
//! port per destination.

use crate::interface::{Interface, InterfaceId, InterfaceKind, NeighborId};
use async_trait::async_trait;
use parking_lot::Mutex as SyncMutex;
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex as AsyncMutex};

type Inbox = mpsc::Sender<(NeighborId, Vec<u8>)>;

/// How a node's source address is translated as seen by its peers.
#[derive(Clone, Debug)]
pub(crate) enum NatModel {
    /// Publicly reachable: peers see the node's registered (internal) address.
    Public,
    /// Endpoint-independent mapping (cone): one external address for every
    /// destination — stable, announceable, directly punchable.
    Cone { external: SocketAddr },
    /// Address/port-dependent mapping (symmetric): a distinct external port per
    /// destination — no single stable external address.
    Symmetric { external_ip: IpAddr, base_port: u16 },
}

impl NatModel {
    /// Whether this node drops **unsolicited** inbound — i.e. inbound from a
    /// source it has never sent to (restricted-cone / symmetric filtering, the
    /// thing hole-punching defeats). `Public` accepts anything.
    fn filters(&self) -> bool {
        !matches!(self, NatModel::Public)
    }
}

#[derive(Default, Debug)]
struct FabricState {
    /// Registered nodes: internal address → (inbox, NAT model).
    nodes: HashMap<SocketAddr, (Inbox, NatModel)>,
    /// Per-node set of addresses it has sent to — the NAT pinholes. A filtering
    /// node accepts inbound only from a source already in its set.
    sent_to: HashMap<SocketAddr, std::collections::HashSet<SocketAddr>>,
    /// NAT bindings: external mapped address → owning node's internal address.
    /// Created on outbound frames so replies route back to the originator.
    bindings: HashMap<SocketAddr, SocketAddr>,
    /// Symmetric-NAT per-(src, dst) external port assignments — a stable binding
    /// allocated on first use, so a symmetric node presents a *distinct* external
    /// port to each destination (deterministically, in dial order).
    sym_ports: HashMap<(SocketAddr, SocketAddr), SocketAddr>,
    /// Directed loss injection: (src, dst) → how many more `ChallengeResponse`
    /// frames to eat. Models an asymmetric flow-throttle clamping right after the
    /// handshake's first packets (the live mobile-carrier failure), so tests can
    /// prove the mutual challenge recovers by RESENDING. Decremented per drop.
    drop_challenge_responses: HashMap<(SocketAddr, SocketAddr), u32>,
}

/// Shared virtual network fabric.
#[derive(Clone, Debug)]
pub(crate) struct SimFabric {
    state: Arc<SyncMutex<FabricState>>,
}

impl SimFabric {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(SyncMutex::new(FabricState::default())),
        }
    }

    /// Create an interface for a node at internal address `addr` with NAT model
    /// `nat`, registering its inbox.
    pub(crate) fn interface(&self, addr: SocketAddr, nat: NatModel) -> Arc<SimInterface> {
        self.interface_with_kind(addr, nat, InterfaceKind::QuicOverUdp)
    }

    /// Like [`interface`](Self::interface) but with an explicit carrier `kind`, so a test can model
    /// the TCP-relay carrier (`QuicOverTcp`) for connection-quality-ladder path ranking.
    pub(crate) fn interface_with_kind(
        &self,
        addr: SocketAddr,
        nat: NatModel,
        kind: InterfaceKind,
    ) -> Arc<SimInterface> {
        let (tx, rx) = mpsc::channel(1024);
        let (close_tx, close_rx) = mpsc::channel(64);
        self.state.lock().nodes.insert(addr, (tx, nat));
        Arc::new(SimInterface {
            id: InterfaceId(format!("sim-{addr}")),
            kind,
            addr,
            fabric: self.clone(),
            inbox: AsyncMutex::new(rx),
            close_tx,
            close_rx: AsyncMutex::new(close_rx),
        })
    }

    /// Arm directed loss: eat the next `n` `ChallengeResponse` frames sent
    /// `src` → `dst` (see `FabricState::drop_challenge_responses`).
    pub(crate) fn drop_next_challenge_responses(&self, src: SocketAddr, dst: SocketAddr, n: u32) {
        self.state.lock().drop_challenge_responses.insert((src, dst), n);
    }

    /// Deliver `frame` sent by internal node `src` toward `dst`. The destination
    /// sees the sender's mapped (external) address; a binding for that mapped
    /// address is recorded so replies route back. Best-effort (dropped if the
    /// destination resolves to no live node).
    fn deliver(&self, src: SocketAddr, dst: SocketAddr, frame: Vec<u8>) {
        let mut st = self.state.lock();
        // Directed loss injection (armed via `drop_next_challenge_responses`).
        if let Some(remaining) = st.drop_challenge_responses.get_mut(&(src, dst)) {
            if *remaining > 0
                && matches!(
                    crate::protocol::NetworkFrame::decode_from_wire(&frame),
                    Ok(crate::protocol::NetworkFrame::ChallengeResponse(_))
                )
            {
                *remaining -= 1;
                return;
            }
        }
        let nat = match st.nodes.get(&src) {
            Some((_, nat)) => nat.clone(),
            None => return, // unknown sender
        };
        let mapped = match &nat {
            NatModel::Public => src,
            NatModel::Cone { external } => *external,
            NatModel::Symmetric {
                external_ip,
                base_port,
            } => {
                // Stable per-(src, dst) external port: assign once (a NAT binding),
                // reuse thereafter. The allocation index (global map size) is
                // monotone, so distinct destinations get distinct ports.
                if let Some(&m) = st.sym_ports.get(&(src, dst)) {
                    m
                } else {
                    let idx = st.sym_ports.len() as u16;
                    let m = SocketAddr::new(*external_ip, base_port.wrapping_add(idx));
                    st.sym_ports.insert((src, dst), m);
                    m
                }
            }
        };
        st.bindings.insert(mapped, src);
        // Record the outbound pinhole: src has now sent to dst.
        st.sent_to.entry(src).or_default().insert(dst);
        // Resolve destination: a registered node directly, or via an existing
        // NAT binding (a reply addressed to a mapped external address).
        let internal_dst = if st.nodes.contains_key(&dst) {
            dst
        } else if let Some(&internal) = st.bindings.get(&dst) {
            internal
        } else {
            return; // no node, no binding → unreachable
        };
        // NAT filtering: a filtering destination drops inbound from a source it
        // has never sent to (unsolicited). The pinhole is keyed by the source as
        // the destination ADDRESSES it — i.e. the sender's `mapped` (external)
        // address, not its internal one. So a node opens a pinhole for a NAT'd
        // peer by sending to that peer's reflexive endpoint (its mapped address) —
        // exactly what hole-punching arranges. (For a public sender mapped ==
        // internal, so this is identical for every non-punch path.)
        let filtered = st
            .nodes
            .get(&internal_dst)
            .map(|(_, nat)| nat.filters())
            .unwrap_or(false)
            && !st
                .sent_to
                .get(&internal_dst)
                .is_some_and(|pinholes| pinholes.contains(&mapped));
        if filtered {
            return;
        }
        if let Some((inbox, _)) = st.nodes.get(&internal_dst) {
            let _ = inbox.try_send((NeighborId(mapped.to_string()), frame));
        }
    }

    fn other_addrs(&self, exclude: SocketAddr) -> Vec<SocketAddr> {
        self.state
            .lock()
            .nodes
            .keys()
            .copied()
            .filter(|a| *a != exclude)
            .collect()
    }
}

/// An [`Interface`] over a [`SimFabric`]. A `NeighborId` is the peer's address
/// string as this node sees it — which, under NAT, is the peer's mapped external
/// address. `remote_addr` parses it back (the reflexive-discovery seam).
#[derive(Debug)]
pub(crate) struct SimInterface {
    id: InterfaceId,
    /// Carrier kind reported by `kind()` — configurable so a test can model the TCP-relay carrier
    /// (rung 0) alongside UDP (rungs 1–2) for connection-quality-ladder path ranking.
    kind: InterfaceKind,
    addr: SocketAddr,
    fabric: SimFabric,
    inbox: AsyncMutex<mpsc::Receiver<(NeighborId, Vec<u8>)>>,
    // Connection-close plane (test injection): `inject_close` pushes a neighbour
    // here and `recv_closed` yields it, so cleanup-on-close can be driven
    // deterministically without modelling real QUIC teardown.
    close_tx: mpsc::Sender<NeighborId>,
    close_rx: AsyncMutex<mpsc::Receiver<NeighborId>>,
}

impl SimInterface {
    /// Simulate the connection to `neighbor` closing — drives recv_closed.
    pub(crate) fn inject_close(&self, neighbor: NeighborId) {
        let _ = self.close_tx.try_send(neighbor);
    }
}

#[async_trait]
impl Interface for SimInterface {
    fn id(&self) -> InterfaceId {
        self.id.clone()
    }

    fn kind(&self) -> InterfaceKind {
        self.kind
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
        true
    }

    async fn connect_endpoint(&self, endpoint: &str) -> io::Result<NeighborId> {
        let addr: SocketAddr = endpoint.parse().map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("bad sim endpoint: {e}"))
        })?;
        // A real QUIC Initial toward `addr` opens our NAT pinhole (records our mapping). Model that
        // outbound so the net-sim doesn't mask a prod no-op: the hole-punch probe relies on connect
        // opening the pinhole. The frame is empty — it decodes to no FrameKind, so the peer drops it.
        self.fabric.deliver(self.addr, addr, Vec::new());
        Ok(NeighborId(addr.to_string()))
    }

    async fn send_frame(&self, neighbor: &NeighborId, frame: &[u8]) -> io::Result<()> {
        let dst: SocketAddr = neighbor
            .0
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("bad neighbor: {e}")))?;
        self.fabric.deliver(self.addr, dst, frame.to_vec());
        Ok(())
    }

    async fn recv_frame(&self) -> io::Result<(NeighborId, Vec<u8>)> {
        let mut rx = self.inbox.lock().await;
        rx.recv()
            .await
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "sim inbox closed"))
    }

    async fn broadcast_frame(&self, frame: &[u8]) -> io::Result<()> {
        for dst in self.fabric.other_addrs(self.addr) {
            self.fabric.deliver(self.addr, dst, frame.to_vec());
        }
        Ok(())
    }

    fn remote_addr(&self, neighbor: &NeighborId) -> Option<SocketAddr> {
        neighbor.0.parse().ok()
    }

    async fn recv_closed(&self) -> io::Result<NeighborId> {
        let mut rx = self.close_rx.lock().await;
        rx.recv()
            .await
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "sim close channel closed"))
    }
}
