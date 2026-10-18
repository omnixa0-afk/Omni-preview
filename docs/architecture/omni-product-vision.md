---
status: approved
related:
  - docs/architecture/networking-overlay.md (networking architecture)
  - docs/decisions/networking-mesh-overlay-adoption.md (ADR)
  - omni-threat-model.md (concrete attack vectors derived from this vision)
---

# Omni: Product Vision

This document describes what Omni is for. Architectural decisions, feature priorities and trade-offs
are checked against it. A decision that does not match the vision either changes or the vision is
updated explicitly, with a rationale.

## 1. Project stage

Omni started as a small project for the maintainers and a few friends and is not yet aimed at wide
deployment. The architecture is nevertheless meant to hold up if a community or open-source
contributors adopt it. This means:
- no components that do not scale (for example a hardcoded server that must be upgraded under load);
- no dependence on an authority that bootstrapped the network;
- no UX that depends on infrastructure that may not exist.

The balancing principle: "delivered v0 for friends" ≤ "v1 ready for a small community" ≤ "scalable
without redesign". Each step includes the next rather than blocking it.

## 2. Audience and use cases

**Primary**: the maintainers and a few friends. Technically aware users who are willing to paste
invite tokens by hand, add bootstrap peers in settings, and accept that "no cloud" means "backups are
the user's job".

**Secondary**: friends of friends, community contributors, open-source hobbyists. They tolerate minor
friction but do not want to read a manual.

**Not targeted** (not an architectural priority):
- mass-market users who expect "sign up with a phone number, done";
- high-assurance communications that require hardware secure enclaves, formal audit, and so on;
- very large scale (a million or more peers is a different class of routing).

## 3. Threat model

**In scope**:
- **A casual eavesdropper** on a shared network (a café, open Wi-Fi).
- **A network operator** (ISP): it cannot see message content and cannot easily build graphs of who
  communicates with whom.
- **Compromise of any single peer**: breaking into one user does not expose the history of others.
- **Compromise of any single bootstrap peer**: a volunteer bootstrap node that is compromised cannot
  see content or impersonate anyone.
- **Casual abuse**: DoS, spam, malicious announces in the mesh.

Omni has no operating company and no central key or metadata store, so there is nothing of that kind
to disclose.

**Out of scope**:
- Targeted attacks on a specific user by a well-resourced adversary (this would require onion routing
  and a hardware enclave).
- A global passive adversary that observes the whole internet at once (this requires cover traffic and
  mix networks, which are too expensive for P2P).
- Endpoint compromise (if the user's computer has malware, no cryptography helps).
- Side-channel attacks on the cryptography (timing, power analysis, and so on). Off-the-shelf
  libraries are used; custom primitives are not built.

**Boundary**: protections that cost nothing in latency, battery, UX friction or code complexity are
included. Protections that carry a significant cost and only help against a very strong adversary are
left for the future.

## 4. Security philosophy

**(a) End-to-end encryption by default, not optional.** There is no UI toggle to turn encryption on.
Encryption is always on, and the user does not have to think about it.

**(b) Conservative defaults, advanced choices tucked away.** The UI has a single default preset.
Advanced options (custom STUN, manual interfaces, and so on) sit behind an expander and are not needed
by most users.

**(c) Free protections only.** A protection is included when it:
- adds no more than 50 ms of latency to 1-on-1 messaging;
- costs no more than 5% battery;
- does not make the UI noticeably more complex;
- does not use more than 50 MB of memory;
- needs no additional infrastructure (server, bridge).

**(d) No silent updates.** Auto-update is opt-in and uses signed releases. Users should know when the
code that handles their messages changes.

**(e) Forward secrecy and deniability** are provided by the Double Ratchet, and this property is kept.

**(f) Identity is a cryptographic key.** Not an email, a phone number or a username (in v1). A
username is a possible future convenience layer; identity remains the key.

**(g) Local-first.** All user data lives on the user's device. Cloud or backup services exist only if
the user explicitly opts in (not in v1).

## 5. Distribution

**Primary**: GitHub releases.
- Signed with a maintainer key (Sigstore, minisign or GPG; to be chosen)
- Release notes for every release
- Eventually reproducible builds, so that binaries can be verified against the published source

**Secondary**: peer-to-peer.
- A friend sending an .exe or .msix is a valid distribution path
- Signatures are verifiable offline, so a user does not have to go online to confirm that a binary is
  authentic

**Tertiary** (later):
- F-Droid, Microsoft Store, Snap for users who prefer stores
- Linux package managers (apt, pacman, brew)

**Update mechanism**:
- v1: manual download and replacement of the exe, opt-in.
- v2: automatic checks for new releases with opt-in installation. Updates are never applied silently.

**Trust root**: the maintainer's public key, which the user verifies offline, through a friend, or
through GitHub or the repository. If the maintainer key is compromised, releases are compromised, and
recovery requires out-of-band key rotation.

## 6. Bootstrap model

**v1**:
- An empty default bootstrap peer list
- A settings UI with a text box "Add bootstrap peer URL+pubkey"
- mDNS for LAN discovery (peers on the same network find each other automatically)
- Manual invite-token paste for cross-network pairing (when both peers are behind NAT and share no
  bootstrap)

**v1.x** (once volunteer bootstrap peers exist):
- A community-curated list of well-known peers, published in the repository and signed by the
  maintainer
- Users can add, remove and reorder entries in settings
- The default list stays empty until it has 3 or more verified entries

**v2** (once infrastructure exists):
- Optional DNS SRV records for peer discovery (if someone owns a domain)
- Possibly a DHT-like distributed peer registry, but only if Sybil resistance can be achieved through
  social-graph weighting (see networking-overlay.md)

**Anti-goal**: a hardcoded address such as "our.com:443" that cannot be disabled or changed.

## 7. Feature scope and phasing

> **Scope status.** The phases below are the original plan and have been overtaken by later work.
> Beyond v0.x the following are implemented: the mesh overlay with NAT traversal and relays (v1.0);
> **group chats** (MLS, `group-crypto-mls.md`), **group calls** (cascaded peer forwarder,
> `group-call-cascade-sfu.md`), group history for new members and after restore
> (`group-history-sync.md`), files in groups with multiple holders (`attachment-content-tree.md`);
> opt-in **search by name** (`find-by-name.md`); encrypted backup and encryption of data on disk
> (`db-at-rest-encryption.md`). In progress: **one Omni-ID on several devices**
> (`multi-device-equal-nodes.md`, stage 1 and 1:1 history reconciliation). Not started: channels,
> disappearing messages, Android.

### v0.x (friends-only stage)

**Required**:
- 1-on-1 chat (E2E)
- 1-on-1 voice call (E2E via WASAPI/Opus)
- 1-on-1 video call (E2E via libwebrtc; H.264 is hardware-only through Media Foundation, plus HEVC/AV1; the build has no software H.264)
- File attachments (encrypted, with quota)
- Contact pairing via invite-token paste
- LAN discovery (multicast mesh announces; the separate mDNS/UDP discovery was removed)
- Multiple profiles per device (a user can keep several identities)

**Known debt** (present but not production-grade):
- The old signaling was awkwardly written and is being replaced by the mesh.
- Audio testing was blocked pending the mesh work.

### v1.0 (release candidate for community adoption)

**Must add**:
- Mesh overlay (phases α+β+γ from networking-overlay.md): full NAT traversal, multi-hop, self-healing
- UI simplification (one preset instead of nested dropdowns)
- Reliable cross-machine pairing without manual configuration
- Mobile readiness (Android), though not mandatory if time runs short

**Should add**:
- Backup / export of user data (encrypted)
- Multi-device sync (carefully; see the constraints section)
- Better diagnostics (observable mesh state, with problems visible to the user)

**Should not add yet**:
- ~~Group chats~~: done ahead of the plan (see the scope status above)
- Channels
- Stickers
- Disappearing messages

### v2.0 (friendly to community contributors)

**Likely additions**, in priority order:
1. ~~Group chats~~: done
2. Multi-device sync (master identity plus per-device sub-keys): in progress, `multi-device-equal-nodes.md`
3. Disappearing messages
4. ~~Username discovery~~: search by name through rendezvous was built instead of mesh announces (`find-by-name.md`)

### v3+ (community scale)

- Channels / broadcasts (one-to-many; efficient mesh fan-out)
- Larger groups (100+ participants; N×N message complexity must be avoided)
- Stickers, emoji, threads, polls (UI features that do not constrain the architecture)
- A wider ecosystem: bridges to other protocols, a plugin system

### Out of scope

- Phone-number or email account creation
- "Sign in with Google/Apple/etc."
- Server-side message storage
- Push notifications through Firebase or APNs without user opt-in (they leak metadata)
- Server-side or cloud processing of message content (summarization, search)
- Advertising or monetization through user data

## 8. Architecture constraints

These constraints follow from the vision and apply to all architectural decisions.

### C1: The architecture allows group chats and channels without a rewrite

The mesh announce protocol already distributes signed records through the network. Group membership
can be expressed as an announce of a group_id with a list of members, signed by the group creator.
Channels work the same way with a broadcast pattern.

The mesh design in networking-overlay.md does not assume 1-on-1 only; the link layer passes any
message. ✅

### C2: Multi-device sync is possible without a server

Approach: identity is a master keypair plus N device sub-keys authorized by the master. Sync messages
travel through the mesh like ordinary messages between co-devices of one user.

The current X3DH supports a master plus devices. ✅

### C3: Username discovery is Sybil-resistant

A username is an opt-in signed claim in an announce. The mesh propagates it and ties the username to a
NodeAddress. A Sybil can claim an unregistered username but cannot take over a registered one, because
the claim is signed with a public key the Sybil does not hold.

The announce protocol is already signed; only a username field is needed when this is added. ✅

### C4: Disappearing messages require an explicit retention API

The storage layer must expose delete-by-age and delete-by-trigger operations. omni-storage does not
have them yet, but adding them is additive. This is v2 work and does not block the architecture. ✅

### C5: Bootstrap peers are added through the UI, not only through config files

Users can add bootstrap peers in Settings without hand-editing JSON files. Hidden sidecar files exist
today, and a UI is still needed. Pending: the UI cleanup pass.

### C6: No recovery master key or backdoor

There are no master keys for recovery and no administrator override. A user who loses all devices
without a backup loses the data. This is intended: no party, including the maintainers, holds a key
that can read messages. The current crypto layer provides this. ✅

## 9. The mesh architecture against the vision

| Vision principle | Mesh architecture | Match |
|---|---|---|
| No dependence on a central server | Anchors are emergent hubs, not a required role | ✅ |
| Free protection (no onion-routing latency) | Direct hole-punching first, relay only as fallback | ✅ |
| Scale-ready | The mesh design handles 10k+ peers with rate limits | ✅ |
| Identity = key | NodeAddress = blake3(pubkey)[:20], derived from the existing X3DH | ✅ |
| E2E by default | The existing X3DH layer is untouched | ✅ |
| Wire-level metadata privacy | One QUIC connection per peer pair, encrypted forwarded tunnels | ✅ |
| LAN works without internet | mDNS interface plus direct QUIC | ✅ |
| Distribution: GitHub and p2p | No infrastructure dependency at runtime | ✅ |
| Bootstrap addition through the UI | A Settings entry for bootstrap peers (phase α-ε deliverable) | ⚠️ pending |
| Future group support | Mesh announce is already a universal mechanism | ✅ |

## 10. Accepted trade-offs

In exchange for privacy, a decentralized design and simplicity, Omni accepts:

- **Slower onboarding** than apps with phone-number signup. A user pastes an invite token at least once.
- **No silent recovery** if all devices are lost. There is no cloud backup mechanism, unless the user
  opts in to an encrypted backup file.
- **Bandwidth overhead** of mesh routing versus a direct connection (small but real, perhaps 5-20%
  extra for announce propagation).
- **Cold-start latency** on the first connection in a new network (a few seconds for mesh stabilization).
- **No central moderation.** Group abuse is handled by the group owner.
- **A smaller user base** at first.

In return:
- No dependence on a single operator
- Resilience against single points of failure
- Privacy by construction rather than by policy
- The ability to self-host on personal hardware
- Open-source code that anyone can audit

## 11. Success metrics

These are directional signals rather than KPIs.

**Tier 1 (v0.x, working for a small group)**:
- A handful of people use Omni regularly for everyday communication
- Voice and video calls work reliably through typical home NATs
- Nobody runs into "it broke, restart it"

**Tier 2 (v1.0, a community appears)**:
- Six months after v1.0, 10-100 active peers in the mesh outside the original circle
- The repository receives its first external contribution
- A volunteer runs the first community bootstrap peer

**Tier 3 (v2.x, sustainable)**:
- Multiple maintainers
- Some users on Android
- Documentation complete enough for a new contributor to start without help

**Tier 4 (v3+, larger)**:
- 1000+ active peers
- Group chats and channels used regularly
- The project keeps working through a month without developer activity

Mass-market scale (millions of users) is not part of this roadmap.

## 12. Non-goals

- **A cryptocurrency**, token or blockchain layer. It adds risk and technical debt.
- **Social network features** (likes, follows, public feeds).
- **Machine-learning features** (smart replies, content summarization, an in-chat assistant). They
  would require server-side processing of E2E content. If ever added, they would run only on-device.
- **A web client.** The browser security model does not suit E2E key management.
- **Federated identity** (login through Mastodon, Matrix and similar). Identity is self-held.
- **Broadcast / webinar** (one-to-many, 50+ viewers as a product class). This is a different
  architectural problem. Group calls for **friend groups** are scaled through a **cascaded E2E
  peer-forwarder tree** (no dedicated media server, E2E preserved; active-speaker/SVC for video). The
  practical limit is downlink: audio supports large groups (tens of participants), video fewer. See
  `docs/decisions/group-call-cascade-sfu.md`. The non-goal is the webinar/broadcast class, not a large
  call among friends.

## 13. Decision-making framework

When deciding whether to build a feature X, the following questions apply:

1. Does X make Omni more centralized? If so, reject it.
2. Does X require the user to think about security? If so, redesign it so it does not.
3. Does X cost more than 50 ms of latency, 5% battery or significant code complexity for a "free"
   protection? If so, defer it.
4. Does X support future group and channel use cases? If it conflicts, defer or redesign it.
5. Does X depend on a privileged maintainer role? If so, reject it.
6. Does X work without internet in a LAN-only deployment? Preferably yes; if not, it is an explicit
   trade-off.
7. Does X scale to 10k peers without redesign? If it needs a redesign, design it now and build it later.

---

Changes to this document go through a pull request with discussion and an updated status header.
