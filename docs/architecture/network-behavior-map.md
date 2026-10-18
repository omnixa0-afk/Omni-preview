# Omni: a Map of Network Behavior (what actually happens at runtime)

**Purpose.** This is a **map**, not a spec. The mesh-layer spec is `networking-overlay.md`; the "why"
lives in `decisions/serverless-connectivity-masterplan.md` and about 30 ADRs next to it. This document
gives a **coherent picture of the runtime**: what happens step by step, in what order, with which
entities, and what it degrades into on failure. Each section points to the authoritative document for
depth.

Intended reading order: addresses, startup, planes, first-contact paths, background workers, failures, leaks.

---

## 0. Entities and addresses

| What | Form | Source |
|---|---|---|
| **MeshNodeAddress** | 20 bytes | node address in the mesh (announce-routed), derived from the node's ed25519 key |
| **Omni-ID** | `omni:<40 hex>` = `sha256(signing_pub ‖ dh_pub)[:20]` | public identity identifier; what is given to a stranger |
| **pair_secret** | 32 bytes | shared secret of a pair, derived from **X3DH** at pairing; root of all per-pair topics |
| **bootstrap secret** | 32 bytes | `derive_rendezvous_bootstrap_secret(nonce)` from the nonce of a mesh invite; temporary (2 h window) |

Key principle: **discovery ≠ identity ≠ relationship ≠ presence** (an invariant from CONTRIBUTING.md).
A network neighbor is a route candidate, not a contact.

---

## 1. Cold start: what `StartMesh` does

Entry point: `app_mesh.rs::start_mesh_internal` → `MeshRuntime::start` (`omni-mesh-runtime`).

1. **A tokio runtime is built** (multi-thread).
2. **`relay_tcp_endpoint`** is resolved from the network-node registry: the first node with a
   `tcp_endpoint` (`app_mesh.rs`). Only if the bind is not loopback.
3. **`select_carriers(policy, …)`** brings up the carriers:
   - `Auto` (default): the **UDP carrier (QUIC-over-UDP) is always started, unconditionally**; the TCP relay
     carrier is **best-effort** (its bind makes a blocking connect to the relay; on failure the node runs UDP-only).
     Note: historically a failure here aborted the WHOLE start; see `bugs/mesh-start-dead-relay-fatal.md`.
   - `QuicForced`: UDP only. `RelayTcpOnly`: TCP relay only.
4. **The multicast interface** is bound (LAN discovery).
5. `Network::new(interfaces)` → **`network.start()`**: announce loops, the dispatcher and the accept loops start.
6. **Connection privacy** (`DirectOnly` / `RelayOnly` / default) is applied; it gates direct punching and
   rendezvous dialing.
7. Durable state is restored: store-and-forward frames, node↔dialog routes,
   **unclosed bootstrap invites** (`reload_persisted_bootstrap_invites`).
8. Background workers start (see §5).

**Note:** the shell calls `StartMesh` **in the background** (not on the UI thread); otherwise a slow
network would freeze sign-in. Shutdown is `MeshRuntime::shutdown()` (abort tasks) plus the runtime's
`shutdown_background()`, so closing does not wait for blocking tasks.

---

## 2. Plane 1: transport and the "connection ladder"

How A reaches B, in decreasing order of quality; alternatives are kept warm and dead rungs are cut off by
liveness. Canon: `decisions/connection-quality-ladder.md`,
`connectivity-upgrade-via-peer-relay.md`.

1. **LAN-direct**: multicast discovery within the local subnet (multicast does not cross routers).
2. **Direct NAT hole punch**: both sides learn each other's reflexive candidates (via
   plane 2) and open simultaneously. `decisions/mesh-hole-punching.md`.
3. **Port self-mapping**: UPnP-IGD / NAT-PMP, **advertised only after AutoNAT confirms it**
   (validate-before-advertise: only a port that someone actually reached from outside is advertised).
4. **TCP-direct**: the "front door" for networks where UDP is unreliable or restricted. `decisions/tcp-relay-fallback.md`.
5. **Relay carrier**: the whole session goes through a relay as an Interface; content-blind (the relay cannot see content).
6. **Multi-hop / peer-as-relay**: any reachable node can forward;
   `mesh-multihop-identity-delivery.md`.

**Dialing a peer's several addresses is a race, not a sequential walk** (RFC 8305, `Network::dial` →
`race_connect`): addresses start in order of preference with a 250 ms offset, the first to succeed
wins, the rest are dropped. A dead address costs 250 ms, not a handshake timeout.
`bugs/dead-ports-queue-in-front-of-the-live-one.md`, section "Sequential dialing replaced by a race".

On top of an established link there are **two data planes**: reliable wire frames (chat, control) and
unreliable media datagrams (audio/video over QUIC DATAGRAM).

---

## 3. Plane 2: rendezvous (how to find out where to dial)

Without previously known addresses, the sides **publish/read sealed beacons** through couriers of
different transport classes (N-of-many: to prevent a meeting, all of them would have to be taken down at once).
Canon: `decisions/serverless-bootstrap-rendezvous.md`, `global-identity-discovery.md`.

### 3.1 Three topic classes

| Class | Topic key | What is published | Who can read it |
|---|---|---|---|
| **Pair** (acquaintances) | `HKDF(pair_secret, 10-min bucket)` | `Beacon` (endpoints, NAT type, TTL, optional relay bundle), **AEAD-sealed**, padded to 256 B, topic as AAD | **only the contact** (holder of pair_secret) |
| **Identity** (lookup by Omni-ID) | derived from the Omni-ID | `IdentityBeacon`, **Ed25519-signed, NOT encrypted**; carries a prekey bundle (X3DH) + endpoints + node | **anyone who knows your Omni-ID** (by design) |
| **Bootstrap** (first contact via invite) | `HKDF(derive_rendezvous_bootstrap_secret(nonce))` | a beacon sealed under the bootstrap secret | **both sides of the invite** (they know the nonce) |

**Rotation:** pair topics rotate about every 10 minutes (buckets), so long-term linkability of a slot is broken.
**Forgery:** an IdentityBeacon is checked against the commitment `sha256(signing_pub‖dh_pub)[:20] == omni_id` and
the signature, so a courier can **drop** it but **not forge** it.

### 3.2 Couriers (who physically carries a beacon)

The courier process `omni-rendezvous-helper` (key-free, untrusted, isolated) handles:
- **ntfy** (HTTP/TLS) and **Nostr** (WSS): the web class, many independent servers;
- **BT-DHT**: delegated to the **separate process `omni-dht-helper`** (a Mainline engine, BEP44
  put/get over a line protocol on loopback). `decisions/omni-dht-helper.md`,
  `bt-dht-rendezvous-courier.md`.

Plus **DHT-over-peer** (`decisions/dht-over-peer.md`): when the node's own UDP is unreliable, `dht_get/put` is sent
**through an already-linked peer** whose DHT works, i.e. "DHT as a service over the mesh". It requires an
existing link, so a cold start with unreliable UDP relies on the web couriers.

**One exe, three processes:** in a single-file build there are no neighboring `.exe` files, so the application
**re-launches itself** with `--run-helper <kind>`, and the couriers remain separate processes.
`decisions/single-file-self-exec-helpers.md`.

### 3.3 What a DHT record looks like

A BEP44 mutable slot: `target = sha1(pubkey ‖ salt)`, where the keypair is derived from the topic secret (with a separate
HKDF info, so that someone who knows the topic cannot overwrite it). **Two slots per pair** (salt `"0"`/`"1"`):
`DhtSlotClaimer` gives each side its own slot **without exchanging identity** (it tries both, takes an empty one,
then sticks to it). The value is `nonce(12) ‖ ciphertext`. A third-party observer sees **an opaque key +
an opaque ~256-byte blob**: no who, no to whom, no addresses.

---

## 4. First-contact paths (and how they relate to couriers)

| Path | UI | Goes through couriers? | Mechanics |
|---|---|---|---|
| **By Omni-ID** | Contacts → "Add" → "By Omni-ID" → "Find and add" | **Yes** | `find_contact_by_omni_id`: fetches the `IdentityBeacon` from the identity topic → checks signature/commitment → takes the prekey bundle → X3DH → creates an outgoing request → dial + a session-bootstrap that **carries the request**. The other side **confirms manually**. Requires the target to have "Findable by ID" enabled. |
| **Mesh invite** | Contacts → "Add" → "Pairing over mesh" | **Yes** | Generation: the bootstrap secret is derived from the nonce, an entry is registered (2 h window) and the bootstrap beacon is **published immediately**. Import: registers the own side. Both sides converge on **one bootstrap topic**. A retry sweep **every 30 s** while any invite is unclosed; survives a restart. `decisions/invite-bootstrap-rendezvous.md`. |
| **Reach code `om3reach:`** | Contacts → "Add" → "Reach code" | **No, intentionally** | "Reach without third parties": the token carries the **current** addresses (UPnP / global IPv6 / LAN), and the friend dials **directly**. Ephemeral: if the network changed, issue a new one. This is a manual path for when rendezvous is unavailable. |

**Synergy:** for **already added** contacts a durable per-pair sweep runs in the background and reunites
unlinked contacts through couriers on its own. The reach code is a manual path on top of that, not a replacement.

**Known gap:** the reach code has no automatic fallback to rendezvous when its addresses are stale; the dial
simply fails.

---

## 5. Background workers and cadences

| Worker | What it does | Cadence |
|---|---|---|
| **Announce** | sends announces (LAN multicast + links), maintains the path table | periodic |
| **Autoconnect** | keeps approved contacts and the relay registry **linked**, redials dropped ones | worker tick |
| **Rendezvous sweep** | for **UNlinked** contacts: publishes my pair beacon + reads the peer's + dials | base interval + **jitter** (anti-fingerprinting) |
| **Early rendezvous retry** | a contact whose last attempt produced no direct link gets two more looks; a degraded link gets one immediately (with a floor against flapping) | +30 s and another +60 s after the attempt, then back to the sweep; checked every tick |
| **Bootstrap sweep** | the same for unclosed invites | **30 s**, only while invites exist |
| **UPnP lease refresh** | renews the mapping before the lease expires | ~½ of the lease |
| **Liveness/presence** | cuts off dead relay paths so the fallback switches honestly | periodic |

Sweep work is proportional to the number of **unlinked** contacts (linked ones are skipped), not "all contacts, always".

---

## 6. Degradation: what turns into what on failure

| Failure | Behavior |
|---|---|
| Relay unavailable | UDP-only mesh (LAN + direct + rendezvous). Start does **not** fail |
| No UPnP / CGNAT | no public port announcement; needs hole-punch or a relay |
| UDP unreliable or restricted | TCP-direct → relay carrier; DHT only via DHT-over-peer (needs a link) |
| Web brokers unreachable | DHT + LAN + manual paths (invite/reach code) remain |
| DHT helper failed to start | the BT-DHT leg is skipped, N-of-many covers |
| Empty bootstrap list | **invariant: works** (LAN + invite) |
| Both sides behind symmetric NAT | discovery will find them, but **data goes through a relay** |

---

## 7. What leaks (honestly)

- **Beacon content** does not leak: pair/bootstrap beacons are AEAD-sealed and padded to the bucket size; topics are blind
  and rotate.
- **IdentityBeacon is public by design**: by enabling "Findable by ID" you accept that anyone who knows
  your Omni-ID will see your endpoints + prekey.
- **Participating in the DHT swarm reveals your IP** to other swarm nodes; this is a property of the BitTorrent DHT, not of the beacon.
  That is why the DHT is **off by default** with an explicit warning.
- **An intermediate node (multi-hop)** sees the fact of forwarding, not content; the trust model is in
  `networking-overlay.md`, section "Trust model".
- The anti-entropy **scope fix** prevents handing out the inventory of group feeds to untrusted peers.

---

## 8. Where to look next

| Topic | Document |
|---|---|
| Mesh overlay spec (announce, path, link, interfaces, trust) | `architecture/networking-overlay.md` |
| Umbrella connectivity plan (4 planes, phases) | `decisions/serverless-connectivity-masterplan.md` |
| Quality ladder / dual carriers | `decisions/connection-quality-ladder.md` |
| NAT hole punching | `decisions/mesh-hole-punching.md` |
| Rendezvous (general model) | `decisions/serverless-bootstrap-rendezvous.md` |
| Lookup by Omni-ID (Model A) | `decisions/global-identity-discovery.md` |
| First contact via invite | `decisions/invite-bootstrap-rendezvous.md` |
| BT-DHT courier / DHT process / DHT-over-peer | `decisions/bt-dht-rendezvous-courier.md`, `omni-dht-helper.md`, `dht-over-peer.md` |
| DHT in an isolated segment (entry node, bootstrap set) | `decisions/dht-bootstrap-mesh-propagation.md` |
| Relays (rotation, trust) | `decisions/r1c-rotatable-white-relay.md`, `connectivity-upgrade-via-peer-relay.md` |
| Connection privacy modes | `decisions/connection-privacy-modes.md` |
| One exe → several processes | `decisions/single-file-self-exec-helpers.md` |
| Known start-up bugs | `bugs/mesh-start-dead-relay-fatal.md` |
