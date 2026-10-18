# Architecture Overview

Omni is built around a Rust core and a Windows shell (WinUI 3). The shell is presentation-oriented; Rust owns product truth.

This file describes the current product boundary.

## Core Principles

- Rust is the source of truth.
- C# is a UI and bridge layer only.
- FFI uses a stable C ABI and UTF-8 JSON payloads.
- discovery is route discovery, not social identity.
- relationship state, trust, meeting state, and presence remain separate concepts.
- the UI still presents one active verified chat/session model.

## Product Model

The repository currently exposes these user-facing state families:

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
- contact flags
  - `Friend`
  - not friend
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

These axes intentionally do not collapse into one mega-status.

## Current User Flows

### Local profile lifecycle

1. The user can create or restore a local profile.
2. Saved profiles can be selected and unlocked later on the same device.
3. Signing out returns the shell to the chooser or welcome flow without pretending the profile disappeared.
4. Deleting from device removes the local profile honestly and returns the app to the correct unauthenticated surface.

### Offline add

1. The user enters `account_id` and a local display name.
2. Rust creates an approved contact immediately.
3. The contact shows `Never met` until a verified secure session actually happens.

### Discovery and intro

1. Discovery yields a cheap LAN hint.
2. Rust may expose the peer as `Seen in LAN` or `Reachable`.
3. A secure intro or explicit connect may attempt session establishment.
4. Identity, approval, and verification are evaluated in Rust.
5. Only approved and verified states can drive normal chat behavior.

### Friend probing

1. Approved contacts may be promoted to `Friend`.
2. Quiet reconnect is reserved for approved-friend paths.
3. Anonymous unknown peers may be listed without being auto-probed.
4. A first verified session clears `Never met`.

### Presence

1. Any secure-session traffic counts as fresh presence activity.
2. When the session goes idle, a lightweight secure service packet keeps presence honest.
3. Rust moves contact presence through `Online -> Recently -> Offline`.
4. When a session is no longer healthy, Rust closes it and the shell reflects the resulting state.

### Secure call control

1. The user can start a 1:1 call only for an approved contact that already has an established verified secure session.
2. Rust owns call eligibility, invite state, reconnect grace, outcomes, and transcript projection.
3. Realtime media must use the V2 native/QUIC media path; JSON/base64 media frames are not a supported transport.
4. The shell renders the call through a global incoming surface plus in-chat call UI.
5. Raw media is not written into chat history or persistent message storage.

## Layer Map

### WinUI shell

`windows-app/`

- renders contacts, chats, settings, devices, files, diagnostics, saved profiles, LAN peers, and call surfaces
- stores presentation-only state
- sends commands to Rust
- never computes trust, routing, or crypto truth locally
- uses section controls instead of one giant inline authenticated XAML tree
- uses a deadline-driven refresh loop with visibility-aware domains
- prefers coarse Rust-side snapshots for transport, discovery, queue, lifecycle, and diagnostics
- keeps realtime media out of the general refresh coordinator

See [winui-shell.md](winui-shell.md) for current shell/runtime details.

### FFI boundary

`core-rust/crates/omni-ffi`

- exports stable C ABI entry points
- uses UTF-8 JSON payloads
- shields panics from crossing into the shell
- keeps ownership rules explicit

### Application orchestration

`core-rust/crates/omni-app`

- composes storage, identity, messaging, routing, and transport
- owns workflow decisions such as contact actions, probing, secure-session transitions, and projections

### Domain and storage

- `omni-types`
  - shared types, typed screen snapshots, call DTOs, and foundational errors
- `omni-identity`
  - account, device, contact, trust, friend, and presence types
- `omni-storage`
  - SQLite schema, migrations, history, queue, contact, trust, profile, and call-summary persistence

### Transport and routing

- `omni-transport`
  - session seam for the mesh: verified connections registered by mesh links, inbound envelope dispatch, send through the mesh carrier or durable store-and-forward
- `omni-network` / `omni-mesh-runtime`
  - the mesh overlay itself: links, carriers, NAT traversal, relay

### Messaging and protocol

- `omni-messaging`
  - encrypted 1:1 flow
- `omni-protocol`
  - versioned DTOs
  - control-plane, session, and call-signaling payloads with explicit product budgets

## Data Ownership Rules

- Rust owns persistent truth.
- WinUI never writes directly to SQLite.
- discovery output is ephemeral runtime state, not a contact record.
- a dial candidate is not enough to claim a contact match.
- unsolicited discovery or early session traffic must not rewrite active chat/session state by itself.
- `Forget` removes the relationship record and local dialog history from Rust-owned persistence.
- if `Forget` happens during a still-live verified secure session, Rust may let that live transport context die naturally before the route/session state fully disappears.
- configured listener/discovery settings may persist, but runtime start is explicit per session.
- password protection is documented as a local unlock gate for the profile and UI.
- hot-path media frames, media transport internals, and low-level call-secret derivation stay outside the FFI contract.
- only compressed call frame batches and product-safe call summaries cross the WinUI bridge.

## FFI Contract Rules

- shell requests are typed and explicit
- Rust validates every input
- Rust returns typed success payloads or typed error envelopes
- private key material and raw session secrets never cross FFI
- hot-path media frames and media transport internals do not cross FFI

For more detail, see [ffi-model.md](ffi-model.md).
