# FFI Model

## Boundary Principles

- Rust core is the single source of truth.
- WinUI is a thin client.
- Cross-language contract uses a stable C ABI plus UTF-8 JSON payloads.
- The shell renders typed projections and invokes typed commands.
- The shell does not compute trust, routing, probing, presence, or crypto/session semantics.

## Ownership And Safety

- the shell passes UTF-8 JSON requests into Rust
- Rust validates input and executes domain logic
- Rust returns UTF-8 payloads or typed error envelopes
- Rust-owned strings are released by `omni_string_free`
- Rust-owned app handles are released by `omni_app_free`
- exports are panic-shielded; panics do not cross FFI

## Error Shape

When an operation fails, Rust returns a typed error envelope similar to:

```json
{
  "ok": false,
  "error": {
    "kind": "invalid_input|storage|transport|internal",
    "message": "..."
  }
}
```

Successful responses are typed JSON snapshots or typed result records.

## Contract Surface By Domain

The exact exported symbol set evolves, but the current surface is grouped like this:

- lifecycle
  - init, free, ping, string-free
- transport and discovery
  - listener control
  - dial/connect
  - transport snapshots
  - discovery snapshots and controls
- account and profile lifecycle
  - create
  - restore
  - list/select/unlock/sign-out/delete previews
- contacts and trust
  - list contacts
  - add by identifier or payload
  - approve/decline/forget/rename
  - get contact cards and verification state
- chat and message projections
  - open/start chat
  - send text
  - list messages
  - header and security summaries
- calls
  - start / accept / decline / hang up call
  - set local mic muted
  - get call inbox snapshot
  - project call summary into chat header and call inbox surfaces
- devices and sync
  - list devices
  - link / accept / finalize
  - revoke / role changes
  - summaries and diagnostics
- routing and diagnostics
  - route summaries
  - direct-route and sync-route diagnostics

The important rule is not the exact symbol count. The rule is that product truth stays in Rust and WinUI receives typed summaries instead of raw internals.

## Realtime Media Boundary

The legacy JSON/base64 call media bridge has been removed. Production realtime media must use the V2 media path: native camera/media handling, RTP over QUIC DATAGRAM, and reliable control for RTCP/session feedback. The FFI bridge must not carry hot-path media frames as JSON.

## Account Lifecycle Semantics Over FFI

- restore is mnemonic-root based
- password unlock is local unlock only
- mnemonic unlock is the ownership and recovery fallback
- dangerous actions use preview APIs and explicit confirmation

This keeps lifecycle meaning consistent in the Windows shell while business logic remains in Rust.

## Data That Must Never Cross FFI

- private keys
- session secrets
- call master secrets
- per-direction media keys and nonce material
- raw ratchet state
- replay-window internals
- raw realtime media frames or RTP packets on the hot path
- low-level crypto state blobs that are not needed for product projections

Only product-safe summaries and typed diagnostics cross the boundary.

## Platform Secret Protection Note

FFI consumers do not implement crypto internals. Platform integration only provides secret-wrap hooks where required by the core:

- Windows target path:
  - DPAPI-backed
- non-Windows dev/test target path:
  - conservative plaintext fallback only for compatibility and tests

This keeps cryptographic decisions in Rust while allowing platform-safe secret handling.
