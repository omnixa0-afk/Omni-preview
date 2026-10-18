---
status: approved architecture; replaces all previous signaling designs
supersedes:
  - docs/architecture/signaling-target-model.md (preset picker, abandoned)
  - earlier draft of this file (anchor-mediated; folded into "anchor as enhancement" here)
related:
  - docs/bugs/signaling-architecture-audit.md (the legacy design being replaced)
  - docs/decisions/networking-mesh-overlay-adoption.md (the ADR recording this decision)
---

# Omni Networking: a Reticulum-style Self-Organizing Mesh Overlay

## Why this and not the old signaling

The old signaling (about 5800 LOC across `app_signaling.rs` and `MainWindow.Signaling.cs`)
accumulated enough legacy workarounds that local fixes make things worse: each
corrected line runs into the next workaround. The audits show this:

- `docs/bugs/signaling-architecture-audit.md` found 10 categories of problems.
- The F1 fix (a single condition) unblocked one branch, but the code path from
  External Address mode never reaches it: `interface_id=""` and `inventory=[]` arrive from C#.
- A full repair would need another 5-10 point fixes, each of which entrenches
  the old architecture.

The decision was to stop patching and move to a mesh overlay: invest in the
right long-term architecture instead of repairing workarounds.

## Vision in one paragraph

The Omni network is a **self-organizing, self-healing mesh** with cryptographic
addresses, a link-layer abstraction (QUIC today, potentially LoRa/serial/etc.
later), announce-based routing without a DHT, and optional volunteer help from
nodes that have a public IP. The network **works even when no node has a public
IP** (via LAN mDNS plus invite-based bootstrap) and **becomes more reliable with
every new node**, especially every new volunteer with a public IP that provides
a stable entry point. Between any two peers a network observer sees **exactly
one QUIC session** (everything is multiplexed inside). The existing X3DH/Double
Ratchet identity layer stays as it was; the network layer is built on top of it.

The conceptual reference is the **Reticulum Network Stack** (RNS). The
implementation is **Rust-native**, with no Python dependency.

## Core properties

**Self-organizing**: each node periodically broadcasts a signed announce
("I exist, my identity is X, hop count from me = 0"). Neighbors cache it and
forward it with an increment. After a few rounds the network knows who is where,
with no central coordinator and no DHT.

**Self-healing**: routes are soft state with a TTL. When a node drops, its
announces stop arriving, routes through it expire, and the network re-routes
through alternative paths. Nobody has to announce that X is down.

**Cheap**: works with zero infrastructure. The minimal configuration is two
peers on one LAN via mDNS. Any peer with a publicly reachable address
automatically becomes a hub for others. This is not a special role, only an
emergent property of being easier to reach.

**Robust per added peer**: every new peer is another entry point and another
potential relay. A Sybil attack is more expensive because a "useful" node needs
real links and real bandwidth, not just fake IDs.

**Wire-level privacy**: one QUIC session per peer pair on each link. Inside it,
all streams (chat, audio, video, files) are multiplexed. An observer sees only
"X talks to Y", not "X talks to Y, Z and W".

**Multi-transport ready**: Link is an abstraction. QUIC over UDP is the primary
implementation. QUIC over TCP, LoRa and serial can be added later; routing,
identity and announces do not depend on the transport.

**Identity unchanged**: the existing X3DH + Double Ratchet crypto stays as is.
The network address is `blake3(pubkey)[:16]`, derived from the existing identity.
This is backward-compatible with contacts already in the database.

## Architecture layers (top-down)

```
┌────────────────────────────────────────────────────────────┐
│  Application layer:                                        │
│  chat messages, voice RTP, video RTP, file transfer        │
│  All multiplexed inside one E2E QUIC session               │
├────────────────────────────────────────────────────────────┤
│  E2E crypto (X3DH + Double Ratchet): existing,             │
│  unchanged. This is the project's own value-add.           │
├────────────────────────────────────────────────────────────┤
│  Virtual link layer:                                       │
│  - opens by destination address (= blake3(pubkey)[:16])    │
│  - encrypted to destination using established crypto       │
│  - resilient: if the path changes, automatic reroute       │
│  - persistent: survives temporary network changes          │
├────────────────────────────────────────────────────────────┤
│  Path routing layer:                                       │
│  - announce protocol (periodic, signed)                    │
│  - path cache (soft state with TTL)                        │
│  - path request/response for unknown destinations          │
│  - trust-weighted next-hop selection                       │
│  - rate limits + reputation tracking                       │
├────────────────────────────────────────────────────────────┤
│  Hole-punching coordination:                               │
│  - peer A and peer B coordinate via common neighbor M      │
│  - M arranges simultaneous-open                            │
│  - direct link established (if NAT types allow)            │
│  - fallback to multi-hop through the mesh if punch fails   │
├────────────────────────────────────────────────────────────┤
│  Interface (link-layer) abstraction:                       │
│  trait Interface { send/recv frames, MTU, BW, latency, ... }│
│  Implementations:                                          │
│  - QuicOverUdp (primary, v1)                               │
│  - QuicOverTcp (fallback when UDP is unavailable, v2)      │
│  - mDNS (LAN discovery only, broadcasts announces)         │
│  - [future] LoRa, Serial, I2P, Bluetooth, ...              │
├────────────────────────────────────────────────────────────┤
│  Transport: quinn (QUIC), existing                         │
└────────────────────────────────────────────────────────────┘
```

## Conceptual model details

### Destination = address

In this model a peer's **address** is `blake3(pubkey)[:16]` (16 bytes = 128 bits
of identity space, more than enough for billions of peers).

Property: **an address cannot be forged**. Any node can verify that a peer
claiming an address actually owns the corresponding pubkey, via
challenge-response during the handshake. A Sybil cannot take over someone
else's address without breaking the crypto.

The current Omni identity (the X3DH long-term key) is the **source** of the
network address. They are not separate identities; they are the same identity
at different levels of abstraction.

### Announce protocol

Each node periodically (every ~5-15 minutes, adaptive: less often when the
network is stable, more often when topology changes) sends an **announce** over
each of its active links:

```rust
struct Announce {
    destination: [u8; 16],          // hash of pubkey
    pubkey: PublicKey,               // raw pubkey (to validate destination)
    advertised_addresses: Vec<SocketAddr>,  // directly reachable (if any)
    hop_count: u8,                   // 0 = the originator, incremented on forwarding
    timestamp: i64,
    signature: Signature,            // signature by destination over all of the above
}
```

On receipt a node:
1. Verifies the signature (otherwise drops).
2. Checks that `blake3(pubkey)[:16] == destination` (otherwise drops).
3. If the destination is unknown, adds it to the path table:
   `destination → (next-hop = the link it arrived on, cost = hop_count+1, ttl)`
4. If the destination is known and the new announce is shorter or fresher than
   the current entry, updates the entry.
5. If the node forwards announces, increments hop_count and re-broadcasts over
   all OTHER active links (rate-limited).

Anti-flood rate limit: no more than N announces per second per link, and no
more than M duplicate announces for the same destination per minute.

### Path table

Soft-state cache: `destination → {next_hop, cost, last_seen, source_link}`.

Entries expire after a TTL (~30 minutes without refresh). An expired entry is
not an error; it only triggers a fresh announce request when needed.

### Path request

When node A wants to reach destination D but D is not in the path table:
1. A broadcasts `PathRequest(D, requester=A)` over all active links.
2. Any node that knows D replies with `PathResponse(D, our_announce_for_D)`.
3. A adds the best response to its path table.
4. If nobody replies before the timeout, D is unreachable and A gives up.

### Virtual link establishment

A wants to connect to D:
1. Look up the path table; use the entry if present, otherwise send a path request.
2. Open `Frame(destination=D, payload=encrypted_handshake_init)` via the
   next_hop from the path entry.
3. Each node on the path forwards fragments by next_hop without reading the payload.
4. D receives, verifies the handshake, and replies.
5. A **virtual link** is established: a sequence of E2E-encrypted frames between
   A and D that technically passes through intermediate hops but is logically
   point-to-point.
6. On top of the virtual link runs an ordinary QUIC session carrying the
   application streams.

**If the path changes** (an intermediate node drops, a shorter path is found),
this is transparent to the virtual link: the next outgoing frame goes via the new
next_hop. The existing QUIC session may suffer (retransmits, higher latency, at
worst a reconnect). This is an acceptable trade-off for self-healing.

### Hole punching

If A sees D in the path table with cost > 0 (not direct), A can try to
**upgrade to a direct link**:
1. Query a common neighbor (a peer both can see).
2. The common neighbor coordinates by telling both sides to open a socket to the
   other's address simultaneously.
3. If the NATs allow it (full-cone / restricted-cone), the direct link comes up.
4. The path table is updated: D is now reachable direct at cost=0, and the old
   next_hop is demoted to a fallback.

### Interface abstraction

```rust
pub trait Interface: Send + Sync {
    fn id(&self) -> InterfaceId;
    fn kind(&self) -> InterfaceKind;  // QuicUdp | QuicTcp | Mdns | LoRa | ...
    fn mtu(&self) -> usize;
    fn bandwidth_estimate(&self) -> u64;  // bits/sec
    fn latency_estimate(&self) -> Duration;
    fn reliable(&self) -> bool;            // built-in retry?
    fn broadcast_capable(&self) -> bool;   // physical broadcast like LoRa air?

    async fn send_frame(&self, peer: NeighborId, frame: &[u8]) -> io::Result<()>;
    async fn recv_frame(&self) -> io::Result<(NeighborId, Vec<u8>)>;

    async fn broadcast_frame(&self, frame: &[u8]) -> io::Result<()>;  // no-op if not broadcast-capable
}
```

V1 implementations:
- `QuicOverUdpInterface`: the main transport; point-to-point links, established neighbors.
- `MdnsInterface`: link-layer multicast on the LAN, for discovery and initial bootstrap.

V2+ (later):
- `QuicOverTcpInterface`: fallback for restrictive networks where UDP is unavailable (corporate networks, captive portals).
- `LoRaInterface`: for off-grid scenarios. Bandwidth ~5 KB/s, latency >100ms, broadcast-capable. Routing adapts: text goes through, audio/video do not.
- `SerialInterface`: point-to-point cable, for minimal setups (field teams, a single laptop).

## How peers help the network

Every peer **automatically** does the following (base behavior, not optional roles):

1. **Forwards announces**: receives an announce, increments hop_count, and
   re-broadcasts it over other links. Cheap (tens of bytes).

2. **Maintains a path table**: knows routes for destinations it has talked to,
   plus neighboring peers learned from announces.

3. **Coordinates hole punches**: if it sees two neighbors that want to connect
   directly, it coordinates a simultaneous-open.

4. **Forwards path requests**: if it receives a "where is D?" request and knows
   D, it replies.

Optionally (a settings toggle):

5. **Forward bulk traffic** for unknown peers: when enabled, the peer acts as a
   relay for virtual link traffic between other peers. Default OFF on mobile
   (battery/traffic); default ON on desktop (cheap there).

6. **Be a discoverable beacon**: a peer with a public IP can advertise its
   public address in announces, so other peers can connect directly without a
   pre-existing link. This is the "volunteer stable entry point". It is simply a
   peer whose `advertised_addresses` is non-empty.

7. **Long-lived cache**: a peer with a lot of storage can keep longer-TTL path
   tables and serve as the network's memory.

There are no special roles. A public-IP peer is just a peer that is easier to
reach, not an "anchor" with a different codebase.

## Trust model

### What peer X sees (intermediate hop)

- **Metadata**: "a frame for destination D arrived from neighbor N", so it knows
  someone is talking to D through it.
- **Timing/size patterns**: it sees when and how many bytes pass.
- **NOT content**: everything is E2E-encrypted to destination D; intermediate
  hops have no keys.

### What X CANNOT do

- **Impersonate**: impersonating D requires D's pubkey.
- **Forge an announce**: announces are signed by the destination; forging one
  means breaking the crypto.
- **Decrypt forwarded traffic**: it is encrypted to the destination; X is a blind forwarder.

### What X CAN do (and how it is mitigated)

- **Drop / refuse to forward**: mitigated by multi-path. If one peer drops
  traffic, a path through other peers is found.
- **Lie in announces** ("I have a path to D with cost=1"): mitigated by
  reputation. If the path does not work when used, the peer's reputation drops
  and its announces are deprioritized afterwards.
- **Eclipse attack** (controlling ALL of the target's neighbors): technically
  possible if the target is connected only to Sybil nodes. Mitigation: peers are
  recommended to connect to multiple peers, especially peers from their own
  contact list. An eclipse through known contacts is a social attack.

### Anti-Sybil

Sybil resistance does not come from DHT positioning (as in Kademlia) but from an
**economic-resource burden**:

- A useful Sybil needs real links and real bandwidth.
- Fake announces without real forwarding are quickly deprioritized by reputation.
- Crypto IDs cannot be chosen to "occupy a position near" a target; there is no such position.
- Routes through the social graph (through your contacts) take priority over unknown peers.

## Bootstrap

The chicken-and-egg problem: the first connection to the network.

**Option 1 (best): LAN discovery via mDNS.** If other Omni peers are on the LAN,
mDNS broadcasts find them immediately, links are established, and the mesh starts.

**Option 2: invite token from a friend.** The invite contains a pubkey plus
optionally preferred bootstrap peers (URL + pubkey of peers the friend knows to
be online). On import, a link is established to the first reachable bootstrap
peer, and announces from the rest of the network arrive through it.

**Option 3: hardcoded fallback peer list.** A small community-maintained list of
well-known public-IP peers, built into the binary. Used only when both mDNS and
the invite failed. No peer on it is privileged; it is only a convenience, like DNS roots.

**Option 4: manual peer URL entry.** The user enters "my acquaintance has Omni at
omni-server.foo:8443, pubkey=Y". A UI affordance for advanced users.

The UI bootstrap flow is simplified: "Connecting to the network..." runs the
sequence 1→2→3 automatically and falls back to 4 if nothing worked, with a hint
to enter an acquaintance's peer address.

## What we build vs use

### Use existing

- **`quinn`**: Rust QUIC, already in use, no changes.
- **`blake3`** for hashing: already in deps.
- **`ed25519-dalek`** for signing: already in deps.
- **`if-addrs`** / `socket2` for interface enumeration: small deps.
- **Concept references**:
  - Reticulum Network Stack documentation (https://reticulum.network/manual/)
  - libp2p Circuit Relay v2 specification (for relay-based connectivity)
  - libp2p DCUtR specification (for the hole-punching coordination message format)
  - None of these is an actual dependency; the ideas are taken and implemented from scratch.

### Build ourselves

| Component | LOC estimate | Where |
|---|---|---|
| `omni-network` crate (entire new layer) | ~5000 | new crate |
| → identity address derivation | ~100 | submodule |
| → Interface trait + QuicOverUdp impl | ~1500 | submodule |
| → mDNS interface impl | ~400 | submodule |
| → Announce protocol | ~800 | submodule |
| → Path table + routing | ~700 | submodule |
| → Virtual link layer | ~600 | submodule |
| → Hole-punching coordination | ~400 | submodule |
| → Public API surface | ~300 | submodule |
| `omni-app` integration | ~500 new / −3000 old | existing crate |
| C# bridge update | ~200 new / −1300 old | existing |
| UI: simplified pairing | ~300 new / −500 old | existing |

**Net change**: +~6000 LOC new, −~4800 LOC old = +1200 LOC. But:
- **functionally** it is an order of magnitude more capable
- **maintenance** is simpler (clear layers, no workarounds)
- **mobile-ready** (the architecture accounts for background/foreground)
- **future-proof** (multi-transport, mesh growth)

### Why not libp2p / iroh / Yggdrasil

- **libp2p**: has relevant components (Circuit Relay v2, DCUtR, mDNS, Kademlia),
  but Kademlia is a DHT mesh, not the announce-mesh model used here, and its
  Swarm/Behaviour layer is opinionated. Only the specs are borrowed.
- **iroh**: DERP relays are fixed infrastructure, not peer-volunteered. Pkarr
  uses the BitTorrent DHT, which does not fit.
- **Yggdrasil**: pure Go (awkward to embed) and needs a separate installation.
  Conceptually close but not in implementation.
- **Reticulum (RNS)**: Python. A 95% conceptual match, but not usable as a dependency.

Mesh routing of the needed kind has to be written in Rust in-house. This is a
**strategic investment** in the Omni networking layer.

## Phased rollout

### Phase α: minimal mesh on IP

Goals:
- New crate `omni-network` with trait `Interface`
- `QuicOverUdpInterface` impl
- `MdnsInterface` impl (LAN announces)
- Announce protocol with signed records
- Path table with TTL'ed entries
- Path request/response
- Basic virtual link (no runtime re-route yet)
- API: `dial(destination) → Link`, `accept_links() → impl Stream`
- Integration into `omni-transport`: a new Connection on top of Link
- Test: two processes with different `--app-dir` find each other via mDNS,
  exchange announces, open a virtual link, and pass bytes

After α the network is **functional on one machine / within one LAN**. NAT
traversal is not part of this phase. Internet connectivity is only through
volunteer peers with a public IP (entered manually in the boot list).

#### Implementation status

Landed in `omni-network`:

- ✅ `Interface` trait + `QuicOverUdpInterface` + `MulticastInterface`
- ✅ Signed announce protocol (canonical hand-rolled wire format)
- ✅ Path table with TTL'ed soft state (populated; **read by Phase β** multi-hop;
  the direct dial in α-3 uses the address book, not the path table)
- ✅ `omni-net-primitives` extracted (QUIC config/socket/verifier + multicast
  socket), shared with `omni-transport`, no duplication (Strategy B)
- ✅ **Data plane on point-to-point QUIC** (no longer multicast broadcast):
  `dial(dest)` looks up advertised addresses from verified announces, connects
  over QUIC, runs a **mutual** identity challenge (both peers prove key
  ownership via a signed nonce, as required by the invariant "chat only for
  approved contacts"), then exchanges `Data` frames over the authenticated
  connection. Data from an unverified or mismatched neighbor is dropped
  (anti-spoof); `Data` that races the handshake is buffered and flushed on
  verification.
- ⏳ **Not yet**: integration into `omni-transport`/`omni-app` (a new Connection
  over Link). The old signaling stays frozen until then.

### Phase β: hole punching + multi-hop routing

Goals:
- Hole-punching coordination protocol
- Successful direct links between NATed peers through a common neighbor
- Multi-hop virtual link routing (forwarding through 2+ hops)
- Path re-route at runtime (if an intermediate node drops, switch silently)
- Bandwidth-aware path selection (prefer direct > 1-hop > 2-hop)

After β the network is **functional through any NAT** given at least one common
neighbor, and can self-heal through alternate paths.

### Phase γ: anti-Sybil hardening

Goals:
- Cryptographic verification of all announces
- Reputation tracking per neighbor
- Rate limits for announces / path requests / hole punch requests
- Trust-weighted next-hop selection (prefer paths through known contacts)
- Adversarial tests: simulated Sybil floods, eclipse attempts, blackhole peers

After γ the network has **production-grade Sybil resistance**.

### Phase δ: UI / UX

Goals:
- Simplified pairing UI: invite token (pubkey + optional bootstrap peers)
- Settings: a "Help the network as a relay" toggle
- Diagnostics: visible mesh state (active links, known destinations, path table size)
- Migration of existing contacts (X3DH identity preserved, network addresses derived)

After δ the **end-user experience is shippable**.

### Phase ε: cleanup

- Delete the old signaling code (5000+ LOC)
- Update CONTRIBUTING.md, tests, docs
- ADR with lessons learned
- Final regression suite

### Phase ζ: Android readiness

- Background service for long-lived links
- Push integration for wake-up
- Battery-aware polling intervals
- Network change handling

## Media pipeline

- The existing media pipeline stays: audio with lipsync (Phase 2c) is already
  shipped and works over the old signaling.
- Audio/video lipsync (Phase 3) is implemented in the native layer. It does not
  depend on signaling and works with any transport.
- Once mesh α is ready, audio moves to the new transport.

The audio work already done remains valid over any networking layer.

## Locked decisions

All open questions are resolved; see the "Locked decisions" table in
`docs/decisions/networking-mesh-overlay-adoption.md`.

Summary:
- **Address**: 20 bytes (`blake3(pubkey)[:20]`)
- **Announce frequency**: static 5 min in Phase α, adaptive later
- **Bootstrap list**: hardcoded const + community PRs, empty in α
- **"Help the network" default**: tiered (cheap forwarding for everyone; bulk relay only on desktop)
- **Backward compat for invites**: hard break, the importer accepts only the new format (no production users, so no migration burden)
- **Multi-device sync**: out of scope for α, a separate ADR in Phase ε+

---

## Bulk plane: the third plane (attachments)

Alongside the two existing planes, a third one now runs on the same verified mesh Link:

| Plane | What it carries | Properties |
|---|---|---|
| Frames (`Link::send`) | chat, control | reliable, per message; each frame is its own uni stream |
| Datagrams (`Link::send_datagram`) | realtime media (opus/RTP) | unreliable, unordered, dropped under loss |
| **Bulk (`Link::open_bulk_stream`)** | **attachment transfer** | **long-lived ordered stream with flow control** |

**Why a separate plane.** Sending an attachment as frames means paying stream
setup per frame and receiving them out of order (frames travel independently),
so the receiver has to hold the gaps in RAM. This does not scale to multi-gigabyte
files. A single ordered stream gives ordering, flow control and retransmission
from the transport itself, so the receiver writes straight through at constant
memory.

**Design.** `BulkStream` is expressed as boxed `AsyncRead`/`AsyncWrite` rather
than a QUIC type: the public mesh surface stays transport-agnostic (a future
radio/serial interface can carry bulk its own way), and quinn does not leak
across crate boundaries. The `Interface` trait methods (`supports_bulk_streams` /
`open_bulk_stream` / `accept_bulk_stream`) default to "not supported", as for
datagrams.

On the QUIC interface this is `open_bi` on the peer's **existing** connection
(no second dial and no handshake) with an `OMNB` prefix, which rejects a bi
stream opened for something else. The accept loop runs on both connection paths,
incoming and outgoing, so either side can start a transfer.

**Limitation by design:** bulk is a DIRECT-path-only plane. The stream lives on
the neighbor's connection, so store-and-forward through a relay is not available
to it; on a break, the existing resume-by-offset applies. Path selection mirrors
`send_failover` but **does not mutate** `PathSet`: a path that cannot carry bulk
is not dead and must not be struck off, since chat depends on it.

Reception goes through the same per-hop authentication as media: only a verified
neighbor can open a bulk stream, otherwise anyone could push a multi-gigabyte
body. The stream is handed upward with the identity of whoever opened it.

Details and history: `docs/bugs/attachment-stall-and-silent-receive-errors.md`.
