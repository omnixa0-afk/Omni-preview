# Security Audit and Threat Model for `omni`

## Executive summary

Current LAN hardening has closed the previously identified `VerificationChallenge` auto-approve path, the attachment-control `Approved` gap, and the broad active-connection `SessionHello` admission rule. The remaining security work is mostly resource-control and policy depth: discovery is still an unauthenticated hint plane by design, some non-attachment metadata events still need explicit relationship-state review, and global quotas for temp/download churn are still limited.

## Scope and assumptions

- In-scope paths:
  - `core-rust/crates/omni-app/src`
  - `core-rust/crates/omni-transport/src`
  - `core-rust/crates/omni-protocol/src`
  - `core-rust/crates/omni-storage/src`
  - `core-rust/crates/omni-ffi/src`
  - `windows-app/src/Omni.Windows.App/Core`
  - `README.md`
  - `ARCHITECTURE.md`
  - `docs/architecture/overview.md`
- Out of scope:
  - external infrastructure, relay/STUN/TURN, and real runtime configs outside the repository
  - CI/CD as a separate attack target
  - the cryptographic strength of the underlying primitives as such
- Assumptions:
  - the application is a Windows-first 1:1 p2p messenger with no central trust authority, where discovery must not be a source of trust
  - the hostile actor is either a malicious peer on the same network / with the same transport reachability, or a peer that has already established a session but is not yet an approved contact
  - local file operations are treated as a local trust boundary, not a remote network vulnerability
  - the assessment is based on the repository code
- Open questions that would materially change priority:
  - whether the product allows non-LAN and internet-exposed peerings by default
  - how unpredictable message IDs are and whether they can be learned through other side channels
  - who exactly initiates the backup/download paths in the UI and whether there is additional validation at the shell/UI level

## System model

### Primary components

- `omni-transport`: libp2p transport, discovery, admission, quarantine, request/response, attachment-transfer stream.
- `omni-app`: session bootstrap, wire dispatch, contact lifecycle, verification, logical events, attachment pipeline.
- `omni-storage`: SQLite-backed contact/session/message/attachment state and local filesystem operations.
- `omni-protocol`: wire schema validation, payload size budgets, protocol invariants.
- `omni-ffi` + WinUI shell: JSON FFI boundary and desktop UI bridge.

### Data flows and trust boundaries

- LAN / peer network -> discovery socket
  - Data: discovery announcements, alias/account hints, listener endpoints
  - Channel: UDP multicast / broadcast / loopback / explicit targets
  - Security guarantees: packet size cap, source filtering, registry cap, quarantine metadata
  - Validation: JSON parse + basic field checks, but no authenticity proof
- Remote peer -> libp2p transport session
  - Data: `SessionHello`, `SessionBootstrap`, encrypted wire envelopes, attachment transfer frames
  - Channel: libp2p TCP/TLS/QUIC/WebSocket + request/response streams
  - Security guarantees: protocol validation, concurrency caps, request queue caps, quarantine, session crypto
  - Validation: `validate_session_hello_v1`, `validate_wire_envelope_v1`, attachment frame limits
- Encrypted application payload -> `omni-app`
  - Data: chat, receipts, verification payloads, profile sync, attachment control events
  - Channel: decrypted wire envelope dispatch
  - Security guarantees: session bootstrap state and `Approved` checks on chat, attachment offer, and attachment control paths
  - Validation: pending peers remain limited to bootstrap/request/presence flows needed for onboarding
- `omni-app` -> `omni-storage`
  - Data: contacts, sessions, message metadata, temp files, backup import/export paths
  - Channel: in-process Rust calls + filesystem
  - Security guarantees: mostly logical invariants and hash/size checks
  - Validation: caller-provided paths are accepted with limited confinement
- WinUI / FFI -> Rust core
  - Data: JSON requests, file paths, config
  - Channel: C ABI + JSON serialization
  - Security guarantees: panic containment via `catch_unwind`, UTF-8 checking
  - Validation: semantic path safety depends on the caller

#### Diagram

```mermaid
flowchart TD
    P["Remote Peer"]
    D["Discovery Socket"]
    T["Transport Runtime"]
    A["App Runtime"]
    S["Storage and Files"]
    U["WinUI and FFI"]

    P --> D
    P --> T
    D --> T
    T --> A
    A --> S
    U --> A
    U --> S
```

## Workspace trust boundaries

Discussion text, files and task descriptions may come from other members. Agent clients treat a line as an instruction only when the core marks it as a command. A cancelled wait can deliver a batch again. Clients deduplicate by group and message ID, then acknowledge the processed batch. See [discussion permissions and delivery](core-rust/crates/omni-app/src/app_workspace_talk.rs).

A user-editable working folder can contain links and names that resemble recovery directories. Recovery validates the journal against a receipt stored outside that folder and performs rollback relative to an open directory. This assumes the application's private receipt storage is trusted. See [file-version recovery](core-rust/crates/omni-app/src/app_workspace_versions.rs).

## Assets and security objectives

| Asset | Why it matters | Security objective (C/I/A) |
|---|---|---|
| Contact approval state | Determines who can become a regular conversation partner | I |
| Session binding (`dialog_key`, remote account/device) | An error in the binding breaks the trust model and routing | I |
| Local attachments / blobs | May contain sensitive user files | C/I |
| Local database and backup bundle | Holds history, contacts, key runtime state | C/I/A |
| Identity / verification payloads | Tampering breaks trust onboarding | I |
| Discovery registry and peer metadata | Affects routing, UX and transport load | A/I |
| Temp download files / profile media | Can cause disk churn and local degradation | A |

## Attacker model

### Capabilities

- Can send discovery packets on the same L2/LAN or to an explicit discovery target.
- Can establish a libp2p session and send validly formed wire/logical payloads.
- Can be non-`Approved` but already a session-capable peer.
- Can spam attachment frames, receipts, profile sync and bootstrap/control events.
- In a local scenario, can supply an unsafe path through the UI/FFI/integration layer.

### Non-capabilities

- Arbitrary code execution on the victim's machine through the considered paths alone is not assumed.
- Breaking cryptography or forging identity signatures without key compromise is not assumed.
- Arbitrary reading of other files without knowing or reaching the corresponding local API path surface is not assumed.

## Entry points and attack surfaces

| Surface | How reached | Trust boundary | Notes | Evidence (repo path / symbol) |
|---|---|---|---|---|
| Discovery announcements | UDP packet to discovery port | Peer -> discovery runtime | JSON hint plane, not authenticated | `core-rust/crates/omni-transport/src/libp2p_discovery.rs` / `process_discovery_packet_v2` |
| Session hello admission | libp2p request-response | Peer -> transport runtime | Admission is limited to the active peer, already verified peer, pending connect, or idle activation path | `core-rust/crates/omni-transport/src/libp2p_runtime.rs` / `session_hello_admission_open` |
| Session bootstrap | Encrypted wire payload | Peer -> app runtime | Auto-creates pending contacts for unknown initiators | `core-rust/crates/omni-app/src/runtime_transport_events.rs` / `WirePayloadV1::SessionBootstrap` |
| Verification challenge | Encrypted wire payload | Peer -> app runtime | Incoming handler fail-closes for non-`Approved` contacts | `core-rust/crates/omni-app/src/runtime_transport_events.rs` / `handle_incoming_verification_challenge` |
| Attachment download request | Logical event | Peer -> app runtime -> file serving | Attachment control events require an `Approved` dialog contact before serving or mutating metadata | `core-rust/crates/omni-app/src/runtime_transport_events.rs` / `attachment_control_event_allowed` |
| Attachment chunks / completion | Logical events + stream frames | Peer -> app runtime -> temp file | Good per-transfer checks, no obvious global quota | `core-rust/crates/omni-app/src/runtime_transport_events.rs` / `persist_attachment_chunk` |
| Public profile sync | Logical event | Peer -> app runtime -> storage | Applies remote profile data without approval gate at dispatch | `core-rust/crates/omni-app/src/runtime_transport_events.rs` / `persist_public_profile_descriptor_sync` |
| Backup import/reset/download save paths | WinUI/FFI request | Local caller -> storage/filesystem | Accepts caller-controlled paths with destructive file ops | `core-rust/crates/omni-app/src/lib.rs` / `import_database_backup_bundle` |

## Top abuse paths

1. Forget / re-request stale session race
   1. an existing transport connection survives a local `forget`
   2. stale encrypted events from the old ratchet race with the new bootstrap
   3. receiver must ignore stale non-`Approved` encrypted payloads without processing them
   4. fresh request approval must establish a new session before chat resumes

2. Session-only attachment resend
   1. attacker establishes a session but remains non-approved
   2. attacker sends `AttachmentDownloadRequest` for a valid `message_id`
   3. receive-side gate must reject it before metadata or file-serving paths run
   4. regression coverage should keep this path closed for all attachment control events

3. Discovery spoof / registry pollution
   1. attacker floods UDP discovery with rotating announce IDs / aliases / endpoints
   2. registry observes and keeps up to the live cap
   3. UI/routing sees polluted candidates or misleading endpoints
   4. result is degraded UX, routing noise, or connect-to-wrong-endpoint pressure

4. Metadata poisoning through non-attachment control events
   1. attacker gets session without approval
   2. attacker sends receipts or profile sync payloads
   3. storage updates delivery/attachment/profile metadata
   4. result is false state, noisy auditing, or user-visible inconsistency

5. Attachment temp-file churn
   1. attacker triggers repeated transfers / chunks / restarts
   2. receiver creates temp files and checkpoints progress
   3. per-transfer limits hold, but global churn accumulates
   4. result is disk I/O pressure, temp-file buildup, and degraded responsiveness

6. Admission widening through session hello
   1. victim already has an active connection
   2. unrelated peer sends `SessionHello`
   3. admission helper must reject it unless it matches an active/verified/pending/idle path
   4. runtime must not accept more verified session state than intended

7. Local destructive path misuse
   1. caller supplies unexpected DB/download/export path
   2. storage import/reset/delete logic uses that path directly
   3. files or directories outside intended app area are copied over or deleted
   4. result is local data loss or accidental overwrite

## Threat model table

| Threat ID | Threat source | Prerequisites | Threat action | Impact | Impacted assets | Existing controls (evidence) | Gaps | Recommended mitigations | Detection ideas | Likelihood | Impact severity | Priority |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| TM-001 | Session-capable malicious peer | Must reach an encrypted session/bootstrap and not be `Declined` | Sends an incoming `VerificationChallenge` to a non-`Approved` contact | Attempt to bypass the trust/onboarding model | Contact approval state, session binding | Incoming and outgoing challenge paths require `Approved`; response path fail-closes for non-approved contact | Keep regression coverage and telemetry honest | Maintain non-approved challenge tests; count unexpected challenge attempts by relationship state | Audit log / metric on challenge for non-approved contact; alert on contact state changes caused by challenge handler | Medium | High | medium |
| TM-002 | Session-capable malicious peer | Needs a session and knowledge of a valid `message_id`; attachment locally serveable | Sends `AttachmentDownloadRequest` without `Approved` and tries to initiate a transfer path | Unauthorized attachment delivery or dialog binding confusion | Local attachments, session binding | Attachment offer and attachment control events are gated for approved contacts; local-serving flag and blob checks exist | Keep dialog/message binding and quota work under review | Maintain the `pending_contact_attachment_control_events_are_ignored` regression; add per-peer transfer quotas | Metric/log on attachment download requests from non-approved contacts; count resend failures by relationship state | Low | High | medium |
| TM-003 | Malicious peer on same network | Discovery reachable on LAN / explicit target | Spoofs discovery announcements and pollutes the candidate registry | UX/routing degradation, possible misdirection | Discovery registry, availability | Packet cap 4096 B, registry cap 256, quarantine threshold 3, hint-plane docs | No authenticity on discovery packet content | Sign or MAC discovery payloads where possible, or strictly downgrade all discovery metadata in UI/routing; add stronger anti-replay / per-source rate limits | Metrics on discovery cardinality, alias/account churn, per-source announce rate | High | Medium | medium |
| TM-004 | Session-capable malicious peer | Has session, not necessarily approved | Sends receipts / profile sync / route hints to mutate local metadata or peer-hints discovery state | False message state, noisy DB churn, user confusion, possible future misdirection via poisoned hints | Message metadata, profile state, peer hints, auditability | Receipts and public-profile syncs already gated to Approved via `dialog_contact_is_approved_for_logical_events`. **RouteHintsSync is now gated to Approved in `persist_route_hints_sync` (TM-004 fix, see `docs/decisions/tm-004-metadata-poisoning-fix.md`)** with regression test `tm004_route_hints_sync_is_gated_on_approved_contact_state`. Chat/attachment event classes were already approval-gated. Verification challenges and challenge responses are signature-validated. | Contact-presence-ping pending-contact creation falls under TM-008 spam scope, not TM-004 | (mitigated) Future: count logical events by relationship state in telemetry | Drops emit `tm004/route_hints/drop` trace events when the `OMNI_TRACE_FILE` env var is set | Medium | Medium | mitigated |
| TM-005 | Session-capable malicious peer | Can repeatedly trigger transfer workflow | Floods attachment frames/restarts causing temp-file and disk churn | Availability degradation, disk pressure | Temp files, storage I/O, app responsiveness | Per-transfer reorder cap 8 MiB, stream frame size caps, request concurrency 4, queue 64, file/hash checks. **Per-dialog quota=8 active inbound transfers**, **global temp-bytes budget=512 MiB**, **stale-transfer GC at 24h TTL** (see `docs/decisions/attachment-transfer-quotas.md`) | None remaining. Future: configurable limits via FFI, per-account counters, adaptive sizing by free disk space | (mitigated) Monitor temp download dir size, active transfer count, repeated failed transfer IDs, attachment GC counter | Medium | Medium | mitigated |
| TM-006 | Malicious or unexpected local caller | Can invoke backup/download/reset APIs through UI/FFI or future integration | Supplies arbitrary filesystem paths to delete/copy operations | Local data loss / overwrite | Local database, filesystem | Foundation helper `confine_destructive_path` (canonicalize + Windows/*nix system-directory blocklist + drive-root rejection + absolute-path requirement) in `omni-storage/src/lib.rs:128`. Sites covered: `Storage::delete_local_data_at_path`, `Storage::save_attachment_copy_to_path`, `Storage::export_database_backup`, `omni-app::export_backup_bundle_contents`, `omni-app::request_attachment_download`. Regression tests at storage layer (`tm006_confine_*`, 4 cases including Windows system paths, drive roots, relative paths, positive temp-dir) and app layer (`tm006_export_database_backup_rejects_system_path`, `tm006_request_attachment_download_rejects_system_save_path`). See `docs/decisions/path-confinement-tm006.md` (foundation) and `docs/decisions/tm-006-destructive-path-confinement.md` (follow-up, closes the last three open sites). | (mitigated) Future: structured logging for destructive path ops with canonical target (TraceLog hookup TBD) | (mitigated) Monitor for `OmniError::InvalidInput("destructive path is in a protected system directory: …")` in error telemetry | Medium | Medium | mitigated |
| TM-007 | Session-capable malicious peer | Victim already has an active connection | Sends unsolicited `SessionHello` while active connection exists | Admission broadening, extra verified session churn | Session binding, availability | Admission now allows only same peer, verified peer, pending connect, or explicit idle activation; regression covers unrelated active-session attempts | Continue monitoring reconnect and discovery-assisted flows | Keep the narrow admission helper tests and avoid reintroducing active-connection wildcard admission | Count admitted session hellos by reason; alert on accepted hello from unexpected peer | Low | Medium | low |
| TM-008 | Session-capable malicious peer | Can reach bootstrap/session or presence ping paths | Creates large numbers of pending contacts/observations | Contact list clutter, storage churn, operator confusion | Contact state, availability | Declined contacts ignored; approved contacts get special handling; storage `add_pending_contact` is idempotent on `remote_account_id` so a single peer can't double-row; **`upsert_transport_pending_contact` gates NEW (no pre-existing) pending-creations through a per-profile sliding-window rate limit: 32 new entries per 60 minutes, beyond which the call silently drops** with a `tm008/pending/drop` trace (see `docs/decisions/tm-008-pending-contact-rate-limit.md`). User-initiated outgoing pendings (`add_outgoing_pending_contact` from signaling paste) bypass the limit by design. Regression tests: `tm008_rate_limit_admits_under_threshold`, `tm008_rate_limit_drops_at_threshold`, `tm008_rate_limit_recovers_after_window`, `tm008_rate_limit_is_per_profile`. | (mitigated) Future: quarantine contact-spam patterns (UI surface beyond rate limit), per-source dedupe by network identity | Drops emit `tm008/pending/drop` traces when `OMNI_TRACE_FILE` is set; rate-limit deque size visible via static registry inspection | Medium | Low | mitigated |

## Bootstrap & community-node threats (TM-009..017)

These cover **cross-network bootstrap, rendezvous and community volunteer nodes** (rendezvous and
relays are implemented; the open volunteer tier, TM-014..017, is still a design). Detail and the per-threat defenses live in `docs/decisions/serverless-bootstrap-rendezvous.md`
and `docs/decisions/community-volunteer-nodes.md`. **Unifying invariant:** trust is identity-bound (the
mutual connect-time challenge, which signs a TLS exporter of its own connection since 2026-10-18 — before
that it could be relayed onto another session, TM-022); every third-party service, the rendezvous **helper process**, and every
volunteer node are *untrusted couriers* of opaque (E2E + signed) blobs under blinded topics, so they can
**DoS or observe metadata, never deceive**. Defenses are deliberately *cheap* (asymmetric crypto-gating +
structural gating + bounded/TTL/backoff), with no PoW and no Tor by default (battery/latency), and **no
tokens**.

| Threat ID | Source / setting | Threat action | Impact | Key defenses (cheap) | Priority |
|---|---|---|---|---|---|
| TM-009 rendezvous poisoning | any third-party rendezvous (Nostr/DHT/ntfy) or the helper | forge/replay a beacon → point a peer at a wrong/attacker endpoint | one wasted dial (not deception) | beacon AEAD-sealed under the pair secret + TTL + nonce (replay guard); the **connect-time challenge**, bound to its own TLS session (TM-022), rejects the wrong peer and cannot be relayed; dedup window | low |
| TM-010 eclipse / provider outage | adversary controls or degrades your rendezvous channels, or a provider is unavailable | isolate a peer from all rendezvous | availability loss | **N-of-many** independent providers; **per-component proxy** (route just the helper via Tor/VPN); mutual-contact rendezvous (in-mesh) | medium |
| TM-011 bootstrap amplification / DoS | malicious peer / open internet | flood your topic; turn you into a reflector | resource/CPU/battery drain | **blinded per-pair topics ⇒ only contacts can reach you**; cheap-verify-before-act; 1-byte probes; anti-amplification dial-back; bounded queues/TTL | medium |
| TM-012 first-contact impersonation | active MITM at the very first key exchange | impersonate a contact during pairing | wrong identity trusted | **irreducible:** out-of-band safety-number / QR verification; loud key-change warning; rendezvous can deliver an invite but never vouches | medium |
| TM-013 helper-process compromise | exploit via hostile bytes from a third-party service | RCE in the untrusted-facing helper | contained: no keys, sandboxed, core unreachable | **privilege separation** (helper holds no keys, sandbox, separate process over omni-daemon IPC) ⇒ degrades to “a malicious courier” = DoS only | medium |
| TM-014 open-relay abuse / DoS | strangers (open volunteer tier) | flood/exhaust an open node’s bandwidth/storage | operator resource drain | per-source fairness (IP+identity) + **local reputation** (not whitelist) + bounded/TTL + **graceful load-shed** (drop open tier first) + content-blind; no-PoW-by-default | medium |
| TM-015 Sybil open-nodes | adversary runs many fake public nodes | attract traffic → observe metadata / selectively drop | metadata exposure, availability | **N-of-many** + **trust-gradient routing** (prefer friend nodes) + health/reputation scoring + identity-bound (can’t deceive) + privacy dial (Tor) | medium |
| TM-016 operator exposure to abuse reports | running an open relay | carry strangers’ (encrypted) traffic → abuse complaints against the operator | operator risk | **content-blind** (can’t see/curate content, like a Tor relay); default **OFF**; informed-consent UX; rendezvous-only low-risk default; easy scope-down + caps | medium |
| TM-017 metadata-to-strangers | open volunteer nodes | see endpoints/timing of strangers’ connections they carry | metadata leak to untrusted operators | **trust-gradient routing** (open = last resort) + **friends-only** privacy cap + **time-bucketed topic rotation** + timing jitter + optional Tor for the helper | medium |
| TM-021 session-bootstrap impersonation | any node that gets a mesh link (LAN, public address, relay) | send a SessionBootstrap that claims a contact's (or our own other device's) account and device | take over the 1:1 session: read outgoing messages, insert ones "from the contact" | **mitigated 2026-10-18:** the bootstrap must carry its device certificate chain (master key → working key → device, the account id being the hash of the master key), and the chain must vouch for exactly the bootstrap's account, device and keys — checked before the bootstrap is recorded, before a dialog route is written for an unknown node, before a session is touched. The newest verified bootstrap per device is kept on disk; an older one (a replay) is ignored. A first-contact bootstrap from a node we have no route for is taken only from the bootstrap's own device node (2026-10-20): a real one relayed by a holder or relay does not bind the dialog route to the relayer | High |
| TM-022 link man-in-the-middle | active attacker on the network path | relay the identity challenge between its own QUIC session with each side | see a link's frames and metadata, drop selectively; read and inject 1:1 call media on a direct path | **mitigated 2026-10-18:** the challenge signs a TLS exporter of its own connection (channel binding); an answer relayed onto another TLS session does not verify | High |
| TM-023 call eavesdropping and injection | a relay on the path; any verified neighbour | listen to or record a 1:1 call; inject audio/video or screen-control input into it | call content and remote input exposed or forged | **mitigated 2026-10-19:** every 1:1 call datagram is sealed in the core with per-direction keys derived from shares carried in the invite and the accept (inside the ratchet session); unsealed, altered or replayed datagrams are dropped; no unencrypted path (`docs/decisions/call-media-e2e.md`) | High |
| TM-024 durable-delivery forgery and crowding | any connection; any verified neighbour or holder | inject durable frames under someone else's address or into someone else's feed; alter or re-age frames it carries; drain other senders' storage quotas | offline messages crowded out or held past their time; forged origin reaches the app | **mitigated 2026-10-19:** a durable frame is signed by its origin node (key hashing to `from`) over the envelope, send time and ciphertext; the feed id starts with the origin's address; frames and anti-entropy are taken only from verified neighbours; lifetime runs from the signed send time, capped at 7 days | Medium |
| TM-025 group-membership disclosure via anti-entropy | any verified neighbour or relay | read the interests in anti-entropy HAVE frames; request held frames by msg_id | learn which groups a node is in, match members; fetch sealed frames of a group it is not in | **mitigated 2026-10-19:** group interests are tokens blinded per pair of nodes (only a member recognises one); a request is served only for frames that would be offered to that peer; verified neighbours only. The number of groups stays visible | Medium |
| TM-026 route poisoning and table exhaustion | a verified neighbour (routes); any connection (announces) | send unasked path replies, replay or re-label announces as direct, announce endless identities | traffic steered through or dropped by the attacker; memory growth | **partly mitigated 2026-10-19:** replies only for our own or relayed requests; announces fresh, ordered, hop 0 only from the peer itself; path table and address book capped. Open: unauthenticated origin of path requests and relayed data (`docs/tasks.md`) | Medium |
| TM-027 undoing a removal by Commit ordering | a group member with a modified client | make Commits for the epoch of its removal until one outranks the admin's | removal, renames and admin changes undone at will | **mitigated 2026-10-19:** a Commit's rank is not chosen by its author — admin before member, then a value from the epoch's exporter and the committer's key; late and forked Commits are ranked the same way on a kept snapshot of their epoch (`docs/decisions/group-crypto-mls.md`) | High |
| TM-028 group eviction by a forged Welcome | an approved contact that knows a group id | send a junk or foreign Welcome for the group over 1:1 | the victim's group deleted before the Welcome is checked | **mitigated 2026-10-19:** a Welcome counts only from the inviter of an accepted invitation; the group's state is replaced only once the new Welcome opened for that very group, otherwise kept | Medium |
| TM-029 forged group history for newcomers | any group member | hand a newcomer history lines signed with its own key under another member's name, at any time | invented lines attributed to others | **mitigated 2026-10-19:** a line counts only from a member known in the group, signed by that member's key, not dated ahead; its name comes from the member's card. Lines of members who left before the newcomer came are dropped | Medium |
| TM-030 safety-number collision | an active attacker at first contact with GPU time | find a key whose safety number matches the contact's | a manual comparison of numbers passes for the wrong key | **mitigated 2026-10-19:** the number is 30 digits (~100 bits) from SHA-512, was 12 (~36 bits); the shell's own check compares keys, not digits. Verified marks are kept by key, so nothing needs re-verifying | Medium |
| TM-031 driving a TCP daemon | another local user; with a non-loopback bind, any host on the network | connect to an `omni-node` TCP endpoint and send commands | the core driven as the user: files read and written through `send-file` / `recv-file`, keys exported | **mitigated 2026-10-19:** every TCP connection proves a token first (`OMNI_DAEMON_TOKEN` or `<app-dir>/daemon.token`, owner-only); a bind outside loopback needs `--allow-remote`. **2026-11-09:** the token never crosses the wire — both sides prove it by challenge-response and the channel after is sealed (ChaCha20-Poly1305, keys from the token and both nonces; `omni-daemon/src/secure_channel.rs`), so an observer of a remote daemon's traffic learns neither the token nor the commands. The shell uses a named pipe for the current user | Medium |
| TM-032 memory exhaustion through a helper | a hostile courier (Nostr relay, ntfy broker) or a compromised helper | answer with an endless line or WebSocket message | the core (or the helper) grows one buffer until it is killed — more than TM-013's "DoS of the rendezvous" | **mitigated 2026-10-19:** the core reads a helper reply at most 8 MiB, its startup lines at 64 KiB; the helper takes a Nostr message at most 256 KiB and an ntfy read at most 2 MiB. Longer is an error, not a truncated reply | Medium |
| TM-033 directory entries pointing into the LAN | anyone who writes a relay-directory shelf (Nostr, ntfy, Mainline) | list a private address in an IPv6 disguise (`::ffff:192.168.x.x`, 6to4, NAT64), or announce one on a Mainline shelf | the reader dials into its own network or host | **mitigated 2026-10-19:** one check, `omni_rendezvous::is_public_ip` — RFC 6890 special ranges, IPv6 forms judged by the IPv4 they carry; Mainline shelf addresses pass it before any dial | Low |
| TM-034 blind request forgery through a contact's couriers | an approved contact | name couriers at addresses inside the victim's network or host (router, local services), or a name that resolves there | the victim's helper sends POST/GET and WebSocket requests into its own LAN | **mitigated 2026-10-19:** a contact's courier URL is kept only if it points out to the internet (public host or non-local name, no user part, web ports); the helper connects such a courier only to public addresses, checked at every resolve, and follows no redirect | Medium |
| TM-035 driving the helpers | any local process; a web page, by a request to the helper's loopback port | send command lines to `omni-rendezvous-helper` / `omni-dht-helper` | the user's ntfy quota burned, records published to the DHT and couriers as the user | **mitigated 2026-10-19:** a random per-process token handed to the helpers at start (`OMNI_HELPER_TOKEN`); every connection opens with `auth <token>`, anything else (an HTTP request included) is dropped unanswered; a helper without a token does not listen | Low |
| TM-036 using a friend's DHT under a forged origin | any verified neighbour, or a relay on the path | send a `DhtProxyRequest` whose `from` names one of the proxy's contacts | the proxy's DHT and per-origin budget used by a stranger, charged to the contact | **mitigated 2026-10-19:** the request is signed by `from` (key derived address, every field but the hop count); unsigned, forged or altered requests are dropped at every hop | Low |
| TM-037 a program dressed as a document | an approved contact or a group member | send an executable or a script under a misleading name (bidi override, a photo named `.hta`) | one click on «Открыть» runs it, with no SmartScreen warning | **mitigated 2026-10-19:** received names are cleaned of controls, bidi and zero-width characters and device names; plain copies leaving our storage carry Mark-of-the-Web; the shell asks before opening a file the core flags as a program (`docs/decisions/received-file-safety.md`) | Medium |
| TM-038 words put in our mouth by 1:1 history sync | an approved contact | hand our device history lines "by us" we never wrote, at any time and dated as it likes | invented outgoing messages in our own copy of the chat | **mitigated 2026-10-19:** from a contact only its own lines are taken; lines by us only while this device is being restored from the phrase (marked «со слов собеседника», checked against the passport); no line more than 10 min in the future | Low |
| TM-039 recorded session starts opened from a stolen database | anyone who copies the database and can open it (malware as the user, a backup) and holds recorded traffic or durable frames | use the prekeys' private halves to redo the X3DH of recorded bootstraps | the first messages of past sessions read | **mitigated 2026-10-20:** the signed prekey rotates weekly and lives 30 days; a rotated one and a taken one-time prekey keep their private half only while a late bootstrap can still name them (a week past expiry / the shared week), then it is erased; an initiator does not bootstrap from an expired bundle (`docs/decisions/prekey-lifetime.md`). Starts younger than that stay exposed | Medium |
| TM-040 keys in the clear and a swapped-in database | anyone who copies the profile folder (Linux/macOS); anyone who can write to it (any platform) | read plain-hex keys off a non-Windows node; plant a key row as plain hex, or a plaintext `omni.db` next to a live `.keys` | the account taken from a folder copy; a database of the attacker's opened as ours | **mitigated 2026-10-20:** off Windows secrets are wrapped with a host-supplied key, fail-closed without it; plain hex only by explicit development opt-in, never on Windows or Android, and read only where it may be written; a plaintext database where the key file says ours was encrypted is refused (`docs/decisions/local-secret-protection.md`) | Medium |
| TM-041 the user's files wiped by a copy, plain sidecars left on a cloud drive | the user's own action (a destination with other files in it), a crash during a save | save a plain database copy into a folder that already holds an `attachments/` or `profile-media/`; a backup save that breaks off half way | unrelated folders deleted recursively; the plain database copy and its sidecars left in a synced folder or on a flash drive | **mitigated 2026-10-23:** a plain copy deletes nothing and is refused where its own files (database, journals, key file, `.bundle/`) already are; the encrypted backup is packed in the profile's own folder and only the finished file reaches the destination, a half-written one is removed | Low |
| TM-042 a stolen device takes the owner's devices back | whoever holds a stolen device (or its working key) | sign revocations of the owner's other devices with the working key and hand them to the contacts | the owner's legitimate devices dropped by the contacts; every `.omnibackup` of the account opens with the same key | **partly mitigated 2026-10-23, accepted risk:** a revoked device is not listened to at all, revocations included; the revoke dialog says that a stolen device needs «Сменить ключ…» (the move to working key n+1, which needs the phrase) and that is what stops the thief. A revocation signed by a still-unrevoked stolen device is honoured until that move | Medium |
| TM-043 the address of a findable account | anyone who knows the Omni-ID | read the `IdentityBeacon` of an account in «Находить меня по Omni-ID» at the couriers (topic from the public Omni-ID) | the IP:port of the account's node and when it is online, without being a contact | **accepted risk 2026-10-23:** the mode is off by default and its switch says that whoever knows the ID sees the connection address; the beacon must carry an address for a stranger's first dial. Moving the address behind an accepted request is a possible later change (`docs/tasks.md`) | Low |
| TM-044 a forged codec passport in a group call | any member of the call | seal a media passport "as" another member (the group media key proves membership only) | the codecs and profile other members send to that member chosen by the attacker (worse quality, a codec it decodes in software, a battery drain) | **mitigated 2026-10-23:** the passport frame is signed with the member's identity key over the call id, its key and the passport; one that does not verify against the sender's key is ignored | Low |
| TM-045 a forwarded or overheard invite makes a stranger a contact | whoever got a copy of an invite (forwarded, shown on a screen) or saw a request answering it (a durable holder, a relay) | import the invite, or move the claim of a seen request into a request of their own | a contact the inviter never approved | **mitigated 2026-10-27:** only the FIRST valid claim approves, then the invite is spent and no longer published; a claim is a PRF of the invite nonce over the request's own signing and ephemeral keys, so a seen claim does not fit another request. Residual: whoever imports a leaked invite before the friend it was meant for is approved — the same window as before, when the inviter could not tell who sent the request either | Low |
| TM-046 one importer of an invite reads another's addresses; a stranger on a link gets our identity | whoever holds a copy of the same invite; anyone who reaches our address (a relay, a neighbour) | open the beacons on the invite's topic (its secret comes from the invite); bring a link up and read the hello we sent on it | the IP:port of the other importers; the Omni-ID, device name, addresses and prekey bundle our hello carries | **mitigated 2026-10-27:** an importer's beacon is sealed to the inviter's per-invite reply key (X25519, carried in the invite), so only the inviter and that importer open it; our hello goes unasked only on a link we brought up or to a peer we know, anyone else gets it in answer to its own hello. Residual: a stranger who sends its hello first gets ours — finding by Omni-ID works that way | Low |
| TM-047 the IP of a node in the Mainline swarm | any Mainline DHT node, any observer of the swarm | see our node store and fetch rendezvous records (on by default since 2026-10-10) | that this IP runs a DHT node and when it is online — as for any BitTorrent client; not whom it looks for (the records are sealed, the topic rotates and comes from a shared secret) | **accepted risk 2026-10-28:** one calm line under «Mainline» in Settings → «Как вас находят» says so; Mainline can be turned off there | Low |
| TM-048 the account root read by a process of the same user | malware or another program running as the same Windows user while the profile has a password | unseal the profile's root secret and phrase with the user's DPAPI | the master key: a move to working key n+1 that takes the account from its owner | **mitigated 2026-10-29:** with a profile password the root lies under the password inside the DPAPI seal (DPAPI(password(root))); a folder copied elsewhere still opens nothing, and the same user without the password gets no root. Residual: such a process still reads the database and the device key (DPAPI), and can capture the password when it is typed (`docs/decisions/multi-device-equal-nodes.md`, 2026-10-29) | Medium |
| TM-049 a repeated nonce in group-call media | anyone on the media path who keeps the frames (a relay, a forwarding member) | record a member's frames from two of its runs in one call (it left and joined again: same key, counter from 0); replay recorded frames into the call | the XOR of two plaintexts and forged frames under that member's key; stale media played again | **mitigated 2026-10-29:** the counter carries an epoch (the current second, never reused in the process) above a 32-bit count, so runs never share a nonce; each receiver keeps per member the newest epoch and a 1024-counter window, older epochs and seen counters are dropped; a call that is gone seals nothing (`docs/decisions/call-media-e2e.md`) | High |

No-privileged-node invariant carried by the community model: **the app must function with an EMPTY
bootstrap list**; no shipped/maintainer node is privileged or required; registries, nodes and providers are
all N-of-many and user-addable/removable.

## Group-call cascade: active-speaker activity leak (C1, accepted)

The cascade peer-SFU (`docs/decisions/group-call-cascade-sfu.md`) gates media at forwarders by a **cleartext
`media_meta` byte** the sender stamps on every media datagram: high bit = is_video, low 7 bits = a coarse
audio-activity level. A forwarder is a mesh peer that carries the call's frames and **may not be a member of
the group**. So `media_meta` reveals to that forwarder **who is speaking + rough loudness** (the low bits) and
**who has video on** (the high bit), per sender, in the clear.

- **Why accepted:** the forwarder already sees the *timing and size* of each sender's datagrams (it routes
  them), so it can already infer the active speaker from traffic shape; the byte is a marginal additional leak,
  not a new capability. Call **content stays E2EE** (the sealed body is never touched; forwarders hold no
  SFrame keys). The alternative, members deciding the active speaker over the control plane and signalling it,
  adds switching lag and its own metadata, and was rejected. This is the price of active-speaker scale
  (bounding per-receiver downlink to ~K streams so **video** scales past the tree's uplink-only fix).
- **Residual / bound:** only *activity + media-kind* leaks, never identity beyond the routing pseudonym
  (`sender_tag`) the forwarder must already resolve to relay. Forwarders are call participants selected by the
  deterministic tree (rank = uplink/willingness), overlapping the trust-gradient routing preference for
  friend nodes (cf. TM-017). Not mitigated further in v1; to be revisited if untrusted open-tier nodes ever carry
  group-call media.

## Criticality calibration

- `critical`
  - a remote path to arbitrary code execution on the client
  - reading or deleting arbitrary local data by a remote peer without strong preconditions
  - a mass trust bypass that automatically turns a stranger into a trusted peer by default
- `high`
  - bypass of the user-approval / trust model for an individual peer
  - unauthorized attachment delivery under reachable preconditions
  - gross violation of session/contact binding that changes the actual recipient/trust
- `medium`
  - DoS or state poisoning without full compromise
  - discovery spoofing / peer pollution / metadata churn
  - locally dangerous path operations if abused by the UI/integration layer
- `low`
  - noisy pending-contact spam scenarios
  - events that break UX but do not compromise anything
  - problems requiring rare or hard-to-reach preconditions

## Focus paths for security review

| Path | Why it matters | Related Threat IDs |
|---|---|---|
| `core-rust/crates/omni-app/src/runtime_transport_events.rs` | Bootstrap, verification, logical events, attachment receive paths and most trust bugs converge here | TM-001, TM-002, TM-004, TM-005, TM-008 |
| `core-rust/crates/omni-transport/src/libp2p_runtime.rs` | Admission logic for `SessionHello`, request-response limits, session lifecycle | TM-007 |
| `core-rust/crates/omni-transport/src/libp2p_discovery.rs` | Unauthenticated discovery ingress, endpoint hints, candidate registry | TM-003 |
| `core-rust/crates/omni-transport/src/lib.rs` | Peer trust hints, quarantine, violation accounting, intro session limits | TM-003, TM-007, TM-008 |
| `core-rust/crates/omni-app/src/runtime_queue.rs` | Outbound attachment transfer path, actual file serving, dialog binding | TM-002, TM-005 |
| `core-rust/crates/omni-storage/src/storage_attachments.rs` | Save paths, temp-file handling, local blob/export logic | TM-002, TM-005, TM-006 |
| `core-rust/crates/omni-storage/src/lib.rs` | Destructive local-data delete helpers derived from a caller-supplied DB path | TM-006 |
| `core-rust/crates/omni-app/src/lib.rs` | Backup import/export entrypoints and delete-before-copy flow | TM-006 |
| `core-rust/crates/omni-protocol/src/lib.rs` | Payload validation budgets and the baseline protections already in place | TM-003, TM-005, TM-007 |
| `windows-app/src/Omni.Windows.App/Core/RustBridge.cs` | UI/FFI path surface into destructive and filesystem-sensitive core operations | TM-006 |
| `README.md` | Documents the intended invariants: approved-only chat, discovery as hint plane, quarantine, soft-fail budgets | TM-001, TM-003, TM-007 |
| `ARCHITECTURE.md` | Records the security model and especially the invariant on unsolicited early session traffic | TM-001, TM-007 |
