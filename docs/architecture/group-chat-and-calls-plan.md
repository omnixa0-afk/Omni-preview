---
title: Groups: chats and calls over the mesh (work plan)
status: in-progress (G0-G10 implemented; remaining: per-sender tiles / active-speaker and a live 3-node A/V gate)
relates:
  - docs/architecture/omni-product-vision.md (scope: groups of 3-8 first; non-goal: 50+ video; C1)
  - docs/architecture/networking-overlay.md (mesh: destination=blake3(pubkey)[:16], announce-routed, multi-hop relay, E2E to the destination)
  - docs/decisions/networking-mesh-overlay-adoption.md
  - docs/decisions/media-hw-efficiency-and-device-codec-selection.md (HW enc/dec and device selection, reused for N incoming streams)
---

# Groups: chats and calls over the mesh

The key principle: **no all-to-all connections (full mesh, Zoom style) for group chat or a shared
call.** This document describes the peer layer (how peers connect and how data moves) separately from
how it looks in the application.

## 0. Scope (from the product vision)

- **Size**: groups of **3-8** members first (v1.1). Channels and 100+ members are a later, separate design.
- **Non-goal**: video calls of **50+** participants. Optimization targets small groups, not conference
  infrastructure.
- **No central server and no central moderation.** Any "server-like" node is an ordinary mesh peer
  chosen by the participants, not a project-operated service.
- **E2E by default, minimal metadata.** Intermediate nodes (including a call's peer forwarder) see
  only encrypted frames plus the minimum routing metadata, never content.
- **The mesh already provides**: destination = `blake3(pubkey)[:16]`, announce-based routing,
  multi-hop relay, store-and-forward potential, E2E up to the destination. The mesh has **no**
  group or multicast address, so fan-out is built on unicast destinations (or a group-destination is
  introduced as a separate primitive, see §3, open question).

## 1. Common foundation: the group as an object (used by both chat and call)

A group is a **signed, versioned object**, not a "chat". Chat and call are both operations on it.

```
Group {
  group_id: [u8;16],             // random; NOT a hash of the members (membership changes)
  epoch: u64,                    // ++ on every membership/key change
  members: [MemberRef],          // identity pubkey + role (admin/member) + added_at
  policy: { who_can_add, who_can_remove, ... },
  founder: pubkey,
  // membership changes are signed messages ordered by epoch
}
```

- Create / invite / leave / kick are **signed group-management messages**, distributed by the same
  fan-out as chat (§3) and applied by `epoch`.
- Stored in `omni-storage` (tables `groups`, `group_members`, `group_epochs`); domain types live in
  `omni-identity` / `omni-protocol`.
- ✅ **Ordering without a server**: `docs/decisions/group-membership-model.md`. Membership is a
  signed, hash-linked causal log that converges through the same anti-entropy as chat. There is **no
  live sequencer**: it would break offline-first operation and introduce a coordinator. Ordering is
  causal with a deterministic tie-break `(lamport, op_id)`; authorization follows the causal role
  state (rooted at the founder); concurrent privileged conflicts converge to one state, and
  equivocation is detected (a fork of the hash chain) rather than splitting the group.

## 2. Group cryptography (E2E for N recipients)

Goal: one logical "send to group" that only current members can decrypt, with key rotation on
membership change (forward secrecy / post-compromise security).

- **Initial design: Sender Keys** (the Signal group model). Each member has a sending key, distributed
  to members over the **existing pairwise secure channels** (X3DH/ratchet 1:1 already exists). A group
  message is encrypted with the sender's key. This is simple, fits the current stack and is cheap for
  3-8 members. The drawback is that a kick requires every member to change their sender key (O(N)
  pairwise rekey), which is acceptable for small groups.
- **Later: MLS (RFC 9420)** if groups grow: O(log N) key updates, tree-based, formally verified. It is
  heavier and is introduced when channels or large groups appear. The membership architecture (§1) is
  built so that MLS can be added without a rewrite (C1).
- Following the vision's "free protections only" rule, Sender Keys are enough for friend groups of 3-8.

## 3. Peer layer: GROUP CHAT over the mesh (not all-to-all)

The problem: if the sender sends N copies directly to every member, the sender's uplink load grows ×N
and there are N independent routes. Offline members receive nothing. The design avoids this.

**Solution: gossip plus store-and-forward over the subgraph of members, keyed by message id (dedup).**

1. A message carries `group_id`, a unique `msg_id`, `epoch`, a signature and a sender-key ciphertext.
2. The sender publishes it to the mesh neighbors/members it **already has a path to** (rather than
   opening N² connections). A member receiving a new `msg_id`:
   - applies it locally (if not seen yet),
   - **re-forwards** it to the other members it has a link to, except the source (epidemic/gossip),
   - dedup by `msg_id` damps the flood, so each link carries the message about once rather than N².
3. **Offline members**: store-and-forward. A node keeps undelivered messages for members it cannot
   currently see and hands them over when their announce reappears (the mesh already keeps a
   soft-state path table). Optionally, dedicated "relay members" with higher uptime act as storage
   points. This is not a server but a group member who voluntarily enabled forwarding, which the mesh
   already supports (default off).
4. **Message ordering**: causal (a Lamport/vector-like stamp on `msg_id`) for "who replied to whom".
   Strict total order is not needed for chat (as in Signal and Matrix). The UI sorts by time and
   resolves concurrency softly.

### Synchronization (catch-up and convergence, no server)

The model is **eventual consistency**. Gossip (§3.1-3.2) is a fast push for whoever is online now, but
it does not guarantee that everyone ends up with the same set (a packet was lost, a member was offline,
joined late, or was in another part of the mesh). Convergence comes from **anti-entropy
reconciliation** on top of gossip. Comparable serverless chats (Briar, Secure Scuttlebutt, Cabal) use
exactly this model.

1. **Per-sender append-only feed.** A message is `{sender_pubkey, seq (monotonic per sender),
   causal_refs (parent msg_ids / per-sender high-water), epoch, ciphertext (Sender Key), sig}`.
   Optionally a **hash chain** (`prev_hash` per feed) detects gaps and forks. Each sender is a strictly
   ordered log; a chat is the union of N logs forming a causal DAG.
2. **Sync state per member** is a compact **high-water vector**
   `{sender → highest contiguous seq I have}` (N numbers), meaning "what I already know".
3. **Two delivery modes:**
   - **Live (push / gossip)**: §3.1-3.2, immediate for whoever is online.
   - **Anti-entropy (pull / reconcile)**: when a mesh link (direct or multi-hop) exists between two
     members, they exchange high-water vectors and **each sends the other what the other lacks**.
     This repairs ANY gap regardless of cause (offline, lost gossip packet, late join). It runs when a
     link is established and periodically.
4. **Store-and-forward bridges non-overlapping online windows.** If A and C are never online at the
   same time but B saw A's message, B delivers it to C (anti-entropy A↔B, then B↔C). Nodes hold
   undelivered messages for absent members; a higher-uptime "relay member" is a convenient (but
   optional, non-server) storage point.
5. **Display order** follows the causal DAG (by `causal_refs`), with concurrent messages tie-broken
   by `(timestamp, msg_id)`. Wall-clock time is not trusted for ordering (clock skew) and is used
   only as a tie-break.
6. **Membership synchronizes the same way.** The group object (§1) is a small signed replicated log
   of membership mutations; the same anti-entropy converges it. A message is bound to an `epoch`, and
   a member can determine whether X was a member at epoch E.
7. **History for a new member** is by default **from the join epoch onward**. This protects privacy
   (the past is not shared with someone who was not there), and after sender-key rotation the new
   member does not have the old keys anyway. Optionally the inviter can explicitly share a limited
   amount of recent history.
8. **Retention / compaction.** Logs grow. When the high-water vectors show that a message is held by
   **all** members, relays and nodes may garbage-collect it; locally the last N / T are kept.
   (Disappearing messages are a separate v2+ feature.)
9. **What a relay / intermediate node sees** is only ciphertext plus the minimum needed for sync and
   routing (`msg_id`, `sender`, `seq`), never content (E2E).

**Edge cases:** (a) a **feed fork**: a sender signed two different messages with the same `seq`
(equivocation), detected by hash chain / seq conflict, flagged and handled by the group owner; (b) a
**gap** in a feed (seq 5 present, 4 missing) is visible immediately from the high-water vector and
closed by the next anti-entropy; (c) **forgery** is impossible because each message is signed by its
sender.

Why not a "mesh multicast destination": a group-destination and multicast routing could be added to
the mesh, but that complicates the overlay core and its metadata profile. For 3-8 members,
gossip over unicast is simpler, works on the current mesh without new primitives, and is better for
metadata (there is no "broadcast" address revealing the whole group). **Open question**: when growing
to channels (one-to-many, 100+), return to a dedicated broadcast/fan-out primitive (the vision allows
this), which is a different design stage.

## 4. Peer layer: GROUP CALL (not all-to-all)

The topology depends on the number of participants **N**.

### N ≤ 3: full mesh
Each participant sends (N-1) streams and receives (N-1). For N=3 that is 2 up / 2 down, which is
cheap and needs no infrastructure or forwarder selection. The existing 1:1 path is reused as is
(RTP-over-QUIC-DATAGRAM × 2). At 3 or fewer participants this is optimal.

### N ≥ 4: peer forwarder (peer-SFU), not full mesh
One member is chosen as the **forwarder** (relay): each "leaf" sends **one** upstream stream to it and
receives from it only what it actually displays. Load: a leaf has O(1) uplink; the forwarder has O(N).

- **It is not a server.** The forwarder is an ordinary mesh peer (a call participant with the best
  uplink and stability, or the initiator). It reuses the existing multi-hop mesh relay and
  RTP-over-QUIC-DATAGRAM.
- **E2E is preserved**: the forwarder **forwards encrypted frames without decoding them**. Media is
  encrypted with a group media key (derived from the group key, §2; frame encryption in the style of
  **SFrame**). The forwarder sees only a thin unencrypted RTP header / layer marker for routing and
  layer selection, never content. This is the "E2EE SFU" pattern (insertable streams, MLS+SFrame).
- **Active speaker plus layered coding (SVC/simulcast)** to avoid overloading each participant's
  downlink: senders encode several layers (temporal SVC is cheap with H.264; spatial via simulcast at
  2 resolutions). The forwarder sends the **full layer only for the participants shown large** (the
  active speaker) and a **low layer / audio only** for thumbnails. Each participant's downlink is then
  about 1 large stream plus k small ones rather than (N-1) full-size ones.
- **Forwarder selection and change**: at call start, signaling (over the group chat / mesh) agrees on
  the topology, measures candidates' uptime and bandwidth, and picks a forwarder. If the forwarder
  leaves, a **new one is selected** (the call degrades for 1-2 s and recovers). If a leaf behind NAT
  cannot reach the forwarder directly, the **mesh multi-hop relay** already solves this (frames pass
  through intermediate nodes).

### Alternatives considered
- **Full mesh for all N**: rejected, because O(N) uplink on every participant overloads a laptop or
  phone at 4-5 participants.
- **MCU (mixing into one stream)**: low downlink but high CPU on the mixer, and it **breaks E2E**
  (the mixer must decode). Not used.
- **A cascade of forwarders (tree)**: suited to 100+ participants. With 50+ video out of scope, a
  single forwarder is enough.

Result: **hybrid, with ≤3 full mesh and ≥4 a single peer forwarder with E2E forwarding and
active-speaker + SVC.**

## 5. In the application (UX)

The UX layer is deliberately separate from the network layer: users do not need to think in terms of
"forwarder" or "layers".

### Group chat
- A thread like a normal one, with N participants: a **member list** (admin/member roles), and system
  messages such as "X added Y", "Z left".
- **Invite** is a signed link / QR (reusing the existing mesh-pairing invite UI; the om3mesh invite).
- Metadata is handled carefully (threat model): typing and read receipts in a group are **opt-in**,
  not default (otherwise "who is online / reading" leaks to N members). The UI shows delivery, not
  presence.
- Following the CONTRIBUTING.md rule that the UI keeps pace with the core, group UI is added only
  when §1-§3 are ready in the core, so there are no "create group" buttons that lead nowhere.

### Group call
- **≤3 (full mesh)**: a grid of equal tiles.
- **≥4 (forwarder)**: **active-speaker layout**, with a large tile for the current speaker and a strip
  of thumbnails. This follows directly from not all streams arriving at full resolution (§4), so UX
  and topology agree.
- **Forwarder indicator**: unobtrusively show who is currently forwarding the stream, plus a badge
  that **the call is still E2E** (the forwarder does not see decrypted video). When the forwarder
  changes, a short "reconnecting…" is shown.
- **Honest degradation**: a participant with a weak uplink gets a lower-quality tile with a marker.
- **Efficiency**: the existing hardware-device selection is reused. **N incoming streams are decoded
  on the iGPU** (more important for N streams than for 1:1), and the self-view is sized to its tile.

## 6. Work stages (depend on mesh maturity)

> Prerequisite: 1:1 chat and call are stable **and** the mesh has reached **Phase β** (multi-hop/NAT;
> before that only single-hop α) **plus a new durable-delivery phase** (persistent store-and-forward
> and anti-entropy; see §7 item 4). Groups come after this (scope v1.1/v2). Each stage ships code,
> tests and docs in one change.

| Stage | What | Layer | Depends on |
|---|---|---|---|
| **G0** | ADR: group object, membership, epoch ordering (sequencer vs CRDT) | design | none |
| **G1** | Domain types and storage (`groups`/`group_members`/`group_epochs`) and membership mutations | identity/protocol/storage | G0 |
| **G2** | Group crypto: Sender Keys (distribution over pairwise channels, rekey on kick) | crypto | G1 |
| **G3** | Group chat over the mesh: per-sender feed, gossip, **anti-entropy reconciliation** (high-water vector), store-and-forward, dedup, causal order; in-process convergence tests (offline / late join) | app/network | G1, G2 |
| **G4** | Group chat UI: thread, members, invite, system messages | shell | G3 |
| **G5** | Group call signaling and ≤3 full-mesh topology (reuses 1:1 media) | app/media | G3 |
| **G6** | Peer forwarder (peer-SFU) for ≥4: E2E forwarding (SFrame), active speaker, SVC/simulcast, forwarder selection/change | media/native/network | G5 |
| **G7** | Group call UI: grid / active speaker, forwarder + E2E indicator, degradation | shell | G5, G6 |

Chat comes before calls (a call reuses the group object and chat signaling), and within calls ≤3
comes before the forwarder (prove the topology cheaply, then scale to 8).

## 7. Design prerequisites

What is not settled before code is written gets fixed into data and protocol formats and is expensive
to rewrite. Items are grouped by how early they block.

### Tier 1: before any group code (they shape the data model and invariants)

1. ✅ **Group trust model**: `docs/decisions/group-trust-model.md`. Group membership is a third axis,
   orthogonal to a 1:1 contact: groups are open / consent-based (the invite is accepted), a co-member
   is not a contact, and a block (`Declined`) applies ONLY to direct messages and 1:1 calls, not to a
   group. The firm guarantee is that membership is not a backdoor into direct messages.
2. ✅ **Membership model**: `docs/decisions/group-membership-model.md` (the G0 ADR). A signed
   hash-linked causal log rooted at the founder, with no live sequencer; causal ordering with a
   deterministic tie-break. Covered: a kick rotates the Sender Key for the remaining members only
   (forward secrecy is not retroactive erasure); re-adding a kicked member is rejected by
   authorization; concurrent admin conflicts converge through the tie-break; equivocation is detected
   (a fork) without splitting the group; partition heal through anti-entropy merge plus a re-converging
   rekey; if all admins leave, membership freezes (an optional recovery policy); invite limits and
   races are a soft gate in the fold; `epoch` is the canonical height, and messages are bound to a
   key epoch.
3. **Wire envelope and metadata boundary.** Exactly which fields are **plaintext** (needed by the
   mesh for routing and anti-entropy: `msg_id`, `sender`, `seq`; whether `group_id` must be in the
   clear) and which are **encrypted**. This threat-model decision underlies everything that follows
   (§3 sync, §4 forwarder). The aim is to minimize leakage of who is in a group and who is speaking.
   The envelope is fixed before anything serializes it.
4. ⛔ **Mesh prerequisites**: a code and roadmap audit established that the mesh did not provide them
   and they were not planned. Groups require persistent **store-and-forward** (a queue of undelivered
   items, limits, GC), an **anti-entropy protocol** (have/offer/request) and **group addressing**.
   In `omni-network`, phase α covers only direct single-hop links (`network.rs`: "α-3 direct only:
   **drop**" for anything not directly reachable; `path_table` is a soft-state stub read only by the
   future Phase β). **Multi-hop is Phase β and was not implemented.** Store-and-forward and
   anti-entropy were not in the α→ζ roadmap (α single-hop, β NAT + multi-hop, γ Sybil, δ UI, ε cleanup,
   ζ Android; see `networking-overlay.md`, "Phased rollout"). This is blocker no. 1 for groups, and
   it is larger than a prerequisite: it needs Phase β and a NEW mesh phase, "durable delivery"
   (persistent store-and-forward plus anti-entropy). Reliable 1:1 delivery to an offline peer needs
   the same store-and-forward. The design is in `docs/decisions/mesh-durable-delivery.md`: a common mesh
   layer of signed opaque per-destination frames with opportunistic store-and-forward and anti-entropy
   through high-water vectors, reused by 1:1 and groups.

### Tier 2: before the corresponding stage

5. **Group crypto specification** (before G2): the exact Sender Key construction, rotation triggers
   (membership plus periodic), the cost of an O(N) pairwise rekey on kick (an acceptable bound?) and
   the post-compromise window; and the **call SFrame key** (before G6), derived from the group key.
6. **Abuse / DoS model for groups** (before G3): flooding a group is **amplified** by store-and-forward
   (one member loads storage and traffic of all). Per-member rate limits and quotas (extending the
   TM-005/TM-008 pattern from 1:1 to groups), backpressure, oversized / replayed messages, feed forks,
   and a kicked member who still relays. Without this, store-and-forward is an amplification vector.
7. **SFrame and SVC/simulcast spike on the H.264 / iGPU path** (before G6, but checked early because it
   can invalidate the forwarder design): temporal SVC through MF/NVENC; where in the pipeline per-frame
   encryption sits (after encode, before packetization), IV/counter, replay; spatial simulcast needs 2
   encoders (CPU/VRAM), so feasibility on an integrated GPU needs checking. A possible start is
   temporal layers plus audio-only thumbnails.
8. **Convergence harness** (before G3): a deterministic multi-node in-process simulation (offline /
   partition / late join / packet loss / reordering) plus adversarial cases (equivocation, flooding).
   Convergence bugs are very hard to catch without a good simulator. It follows the style of the
   mesh-spike tests.
9. **NAT and forwarder selection** (before G6): a leaf behind symmetric NAT reaches the forwarder
   through mesh multi-hop (available), but latency grows, so the forwarder is the node with the best
   reachability and uptime. The selection protocol and failover (what happens to the call during
   re-selection: freeze or drop). Active-speaker detection (by audio energy): where it is computed,
   how it is signaled, privacy.

### Tier 3: product decisions that affect the protocol (they determine what goes on the wire)

10. **History on join**: from the join epoch, with optional limited sharing by the inviter (affects
    crypto, sync and privacy).
11. **Presence / typing / read receipts in a group**: opt-in. The decision determines whether these
    signals are sent at all (otherwise metadata leaks to N co-members).
12. **Retention / compaction / disappearing messages**: the retention policy; the seam for
    disappearing messages (v2) must not block the schema.

### Cross-cutting
- **Android readiness** (CONTRIBUTING.md): the sync engine, group crypto and storage live in the Rust
  core (platform-neutral); native / Windows-specific code is confined to the media forwarder behind an
  interface seam.
- **Channels / 100+ / broadcast** are a separate design (a broadcast primitive in the mesh), kept
  separate from small groups and not anticipated in G0-G7; the vision allows adding them later without
  a rewrite.

## 8. Check against the product vision

- ✅ Groups of 3-8 first; 50+ video out of scope (one forwarder, no cascade).
- ✅ No central server (the forwarder is a chosen peer, not infrastructure).
- ✅ E2E and minimal metadata (SFrame through the forwarder; gossip without a group broadcast address).
- ✅ C1: membership as a "signed announce of group_id plus a member list", as in the vision; MLS and
  channels can be added later without a rewrite.
- ✅ Reuses what exists: mesh relay, RTP-over-QUIC-DATAGRAM, HW enc/dec and device selection, the
  pairing-invite UI.

## Implementation status

A ✅ means the component exists and is covered by unit tests. The implementation ADR is
`docs/decisions/group-envelope-and-transport.md`.

- **G0** ✅: the membership, trust and durable-delivery ADRs exist; the envelope and transport ADR
  closes Tier 1 item 3.
- **G1** ✅: `omni-protocol/group_membership.rs` (hash-linked signed op log and a pure deterministic
  fold: causal authorization, `(height, op_id)` tie-break, equivocation detection, soft member limit,
  freeze at 0 admins, founder guard) and the `omni-storage` tables `groups`/`group_ops`/`group_members`/`group_sender_keys`.
- **G2** ✅: `omni-crypto/crypto_group.rs` (Sender-Key AEAD plus an inner Ed25519 signature, a static
  state key for the log, sealed-box key distribution) and `omni-protocol/group_wire.rs` (feed/ops DTOs
  and the 1:1 consent DTO).
- **G3** ✅: a refinement of §3 above: **one copy per link** rather than N unicast copies (the durable
  store dedups by `(feed,seq)`; N copies would multiply traffic and leak the member list). A synthetic
  group-destination, subscription-based delivery, gossip re-forwarding and anti-entropy
  (`omni-network` `subscribe_group_destination`, `group_destination_address` / `group_feed_id`).
  App orchestration in `omni-app/app_groups.rs` (create / send / invite / welcome + sealed key /
  ingest). An end-to-end app test of convergence.
- **G4** ✅: FFI `omni_groups_*` (create / list / list_members / send_text / invite) and a WinUI
  "Groups" section (list, create, thread, members, invite).
- **G5/G6** ✅ (control plane and crypto): `omni-protocol/group_call.rs` (topology ≤3 full mesh / ≥4
  peer-SFU, deterministic forwarder selection and coordinator-free failover, `media_send_targets`: a
  leaf has 1 uplink, the forwarder fans out) and `omni-crypto` SFrame (`seal/open_group_call_media`,
  a thin plaintext header, the forwarder routes without decrypting).
- **G7** ✅: the call button starts / leaves a real call and shows the topology.
- **G8** ✅: `omni-app/app_group_calls.rs` is the call control plane (start / leave / snapshot) over
  the group feed (Control inner under the sender key); a registry (cleared on profile change) with
  call_id / topology / forwarder / per-sender media key; adoption of a call from an incoming Invite;
  deterministic forwarder failover on Leave. FFI `omni_groups_{start_call,leave_call,call_snapshot}`
  and C#.
- **G9** ✅: peer-SFU media routing: a routing envelope (magic + call_tag + sender_tag,
  `omni-protocol`), `send_group_media_batch` (seal per sender and route by `media_send_targets`), mesh
  `on_media` re-addressing through the forwarder WITHOUT decrypting (E2E), and opening co-members'
  frames in the group thread. Unit tests (envelope, participant open, forwarder target exclusion).
- **G10** ✅: the native RTP pipeline is attached. `send/drain_realtime_media_datagram` for
  `group:<id>` route into the group peer-SFU path (bypassing 1:1 approval); inbound goes through
  `push_incoming_group_media_datagram` (no 1:1 negotiation, since the frame is already E2E-validated);
  `publish_group_media_capabilities` (self-negotiation without a pairwise exchange). C#
  `CallVideoService.StartGroupMediaAsync` and `WatchDialogAsync(groupMode)` skip the 1:1 loops
  (control / capability / negotiation). Test
  `group_call_media_routes_through_group_path_not_1to1_approval`. **v1 boundary:** one mixed remote
  view (SSRC demux by the native receiver); per-sender tiles, active speaker, SVC and a **live 3-node
  A/V gate** remain as UI and validation work.

## Multi-party UI toward a conferencing view (after G10)

Refining the group view toward a conferencing-app level, in increments on top of G10:

- **Increment 1: tile grid** ✅ an adaptive grid (scaled by participant count), manual focus / pin and a filmstrip.
- **Increment 2: audio-level "speaking"** ✅ self RMS plus per-sender decode-only level probes for
  remote participants, drawn as a ring on remote tiles. Speaking is derived **locally from decoding**,
  not over the network.
- **Mute / camera badges** ✅: first for self (a local toggle gates audio sending), then **broadcast of
  other participants' state** through the control plane: a new `GroupCallControlKindV1::MediaState`
  (`omni-protocol`) carries the sender's `muted` / `camera_off`. `app.set_group_call_media_state`
  sends the full current state on every toggle (idempotent) and updates the sender's own registry
  entry; ingest updates the entry of **the sender only** (attribution is the verified sender_pub of
  the feed, so a member cannot forge another's badge); the snapshot carries `is_muted` /
  `is_camera_off` per participant. Defaults are audio-first: `muted=false`, `camera_off=true` (camera
  is opt-in), so the initial state is correct **without** an initial sync. FFI
  `omni_groups_set_call_media_state` and C# (the snapshot is polled at a coarse ~0.5 s cadence and
  updates remote badges in place; self is updated from local toggles every tick). Test
  `group_call_media_state_broadcast_and_ingest_badge_only_sender`.
  - **Speaking is deliberately not broadcast** over the durable feed: it changes about every 120 ms
    and would flood the signed store-and-forward ops feed, and it is already solved locally by the
    decode probes. If an authoritative speaker signal is needed (for auto-focus of the loudest
    speaker), it would travel over the **ephemeral** media path, not this feed.
  - **A late joiner and state from before joining** ✅. Control frames are durable with anti-entropy,
    but durable GC compacts a member's latest `MediaState` as soon as all CURRENT participants have
    received it, so a late joiner would not see "camera on" if it was switched on before joining.
    There was also no live "join" signal: a member adopts a call silently from an `Invite`. The fix
    is symmetric: (1) when adopting a call, publish a `Join` (a previously dormant enum variant that
    carries the secret, so a late joiner can also adopt from it); (2) a member already in the call
    re-announces its **non-default** media state on `Join` / `Invite` (the audio-first default is
    assumed by the joiner anyway, so a no-op is not sent). `handle_group_call_control` now RETURNS
    control frames to publish, and the wire-ingest worker (which has the mesh handle, unlike
    storage-level ingest) sends them through `publish_group_call_control_via`. The publish chain is
    factored into free functions (`publish_group_inner_via` and others), with one body for `&self`
    and the worker. Test `group_call_join_triggers_non_default_media_reannounce_and_adopt_emits_join`
    (it caught a real bug: the secret guard sat before the re-announce branch, making it unreachable
    for Join). Visual verification on a remote tile needs a second participant.
- **The "speaking" ring fades in silence** ✅: the native RMS level was smoothed with
  fast-attack / slow-release **per block**, so when the sender stopped sending audio (pause, Opus DTX,
  mute, leaving) the probe froze at its last value and the ring on the remote tile did not fade. The
  fix: the release continues **by wall clock** on read. `OmniUpdateSpeakingLevel` stamps the block
  time and `OmniReadSpeakingLevel` decays by `0.9^((elapsed−grace)/10ms)` (a 30 ms grace covers the
  10 ms APM / ~20 ms Opus frame plus jitter, so active speech is not underestimated). It applies to
  both probes (self capture and remote playback); self does not regress, since frames arrive
  continuously and the grace does not trigger. `native/omni-webrtc-media` (Release plus libwebrtc
  smoke ✅). The visual fade on a remote pause can only be fully verified with a second participant.
- **Remaining for a conferencing-level view:** auto-focus on the loudest speaker (auto-pin with
  hysteresis; needs a setup with live speakers) and **increment 3**, the live 3-node A/V gate (needs a
  second .NET machine with a camera).
