# Architecture

Omni is a Rust-first product with a WinUI shell. The shell is presentation-oriented; Rust owns product truth.

The guiding rule is simple:

- Rust owns transport, discovery, identity, trust, storage, crypto, routing, and projections.
- WinUI renders typed projections and sends explicit user intent.

## Core Invariants

- the Rust core is the source of truth
- the shell does not own protocol, trust, routing, queueing, or crypto rules
- discovery is not identity
- identity is not relationship state
- relationship state is not presence
- presence is not message history
- a nearby peer is a route candidate, not a trusted contact
- password protection is a local profile gate, not same-user malware hardening

## Main Layers

- `windows-app/`
  - WinUI 3 shell
  - renders chats, contacts, settings, devices, files, diagnostics, saved profiles, LAN peers, and call surfaces
  - talks to Rust through FFI only
- `core-rust/crates/omni-ffi`
  - stable C ABI
  - JSON request/response boundary for C#
- `core-rust/crates/omni-types`
  - shared product types, screen snapshots, call DTOs, and foundational errors
- `core-rust/crates/omni-app`
  - orchestration layer
  - composes storage, transport, identity, messaging, routing, and projections
- `core-rust/crates/omni-transport`
  - session seam for the mesh: verified connections registered by mesh links, inbound envelope dispatch, send through the mesh carrier or durable store-and-forward (the pre-mesh QUIC listener/dialer is gone, `docs/decisions/no-friend-tier.md` part 2)
- `core-rust/crates/omni-storage`
  - SQLite schema, migrations, persistence, queue/contact/history storage
- `core-rust/crates/omni-crypto`
  - crypto primitives and local secret handling
- `core-rust/crates/omni-identity`
  - account, device, contact, trust, friend, and presence domain types
- `core-rust/crates/omni-messaging`
  - encrypted 1:1 envelope flow
- `core-rust/crates/omni-protocol`
  - versioned DTOs and control-plane payload contracts
- `core-rust/crates/omni-sync`
  - device and sync domain helpers

## Active Session Boundary

Transport may keep multiple verified peer contexts alive; Omni still exposes one active verified chat/session model to the shell.

That means:

- discovery may observe many peers
- Rust may keep many candidate routes and peer hints
- the UI still reasons about one active secure conversation at a time

Important consequence:

- discovery or unsolicited early session traffic must not overwrite active chat/session state by itself
- promotion into active state must come from explicit connect intent, a matching pending intro, or an approved-friend reconnect path

This is a deliberate product boundary, not an accident.

## Discovery, Identity, And Trust

These layers are intentionally separate:

- discovery
  - who is nearby
  - how to dial them
  - whether they look reachable
- secure session / handshake
  - who the peer cryptographically claims to be
  - whether session bootstrap and verification succeeded
- trust
  - whether the peer is approved
  - whether the identity is verified in Omni terms

Discovery is treated as a hint plane:

- useful for candidate collection
- cheap to observe
- limited in consequence

Discovery is not allowed to act as:

- contact truth
- account truth
- trust proof
- direct authority over active session rebinding

## Current State Axes

The product keeps these state families independent:

- node mode
  - `Online`
  - `Observe`
  - `Offline`
- discovery publish mode
  - `Off`
  - `Standard`
  - `OmniFlag`
  - `SeeMe`
- legacy explicit probe mode
  - `Off`
  - `OmniFlag`
  - `FriendIds`
  - `Global`
- relationship state
  - `Pending`
  - `Approved`
  - `Declined`
- friend flag
  - off
  - on
- meeting state
  - `Never met`
  - met
- presence
  - `Online`
  - `Recently`
  - `Offline`
- call state
  - `Calling`
  - `Ringing`
  - `Connecting`
  - `Active`
  - `Reconnecting`
  - `Ended`

These do not collapse into one mega-status.

## Contact And Presence Rules

Important rules:

- only approved contacts may open a normal chat
- only approved contacts on an established verified secure session may start or receive a normal call
- offline add by `account_id` creates local `Approved` immediately
- `Never met` clears only after a verified secure session
- `Friend` is a local flag on top of `Approved`
- presence comes from secure-session activity and lightweight secure service traffic, not from discovery alone
- forgetting a contact removes relationship state and local dialog history
- if forgetting happens while a verified secure session is still live, Rust may let that live session disconnect naturally before the transport context fully disappears so later re-request flows remain honest

## Call And Media Rules

Important rules:

- the shipped call surface is 1:1 only, even though call contracts already use participant rosters
- v1 enforces exactly two participants at the app/runtime layer
- only one ringing or active call is allowed per local profile
- unanswered calls time out after `30 s`
- active calls get a `10 s` reconnect grace before ending as `ConnectionLost`
- Rust owns call eligibility, call state, media admission, history projection, and network-quality truth
- production realtime media must use the V2 native/QUIC media path
- JSON/base64 call media, local Opus bridge loops, and request-response media payloads are not supported
- raw media is not stored as chat history
- media transport internals, ratchet state, and low-level call secrets do not cross FFI

## Resource-Control Rules

Omni hardens the LAN path by bounding work instead of trusting discovery.

Current design expectations:

- bounded transport backend queue instead of unbounded accumulation
- explicit protocol budgets on control-plane payloads
- soft failures for invalid or oversized payloads
- noisy-peer accounting and quarantine
- minimal work before a peer becomes authenticated and approved

This is the boundary that keeps hostile or noisy peers from turning discovery noise into expensive state churn.

## WinUI Shell Shape

The Windows client still uses one top-level `MainWindow`, but the shell is split into section controls instead of one giant inline XAML surface.

- `MainWindow.xaml`
  - shell composition only
- `MainWindow.xaml.cs`
  - window lifecycle and shell orchestration only
- section controls
  - onboarding
  - chats
  - contacts
  - devices
  - files
  - settings
  - diagnostics
- support feature modules
  - calls
  - LAN peers
  - saved profiles

See [docs/architecture/winui-shell.md](docs/architecture/winui-shell.md) for the concrete shell/runtime behavior.

## FFI Boundary

The shell may:

- switch configured modes
- request actions
- select contacts and chats
- render summaries and diagnostics
- render call inbox/header projections
- pass only product-safe call/media commands, snapshots, and diagnostics; hot-path media frames do not cross as JSON

The shell may not:

- redefine trust or routing truth
- invent presence locally
- bypass Rust persistence or transport rules
- own raw session or secret state
- own call eligibility or media-key truth

## Runtime Notes

Important current runtime rules:

- entering an authenticated profile does not auto-start listener or discovery
- configured network modes persist, but runtime start is explicit per session
- hidden shell sections use wake-only refresh instead of hot polling
- call summary refresh is wake-only while idle and fast only during ringing, active, or reconnecting calls
- raw audio loops run in a dedicated WinUI call audio service instead of the refresh coordinator
- transport/discovery/queue diagnostics prefer coarse Rust-side snapshots
- first chat load still uses a full history read for stability
- steady-state chat refresh uses Rust-side deltas and revision-gated detail refreshes

## Practical Rule

If a change crosses multiple layers:

1. put truth in the lowest layer that owns it
2. expose orchestration from `omni-app`
3. expose FFI only if the shell actually needs it
4. keep WinUI as presentation and command dispatch
