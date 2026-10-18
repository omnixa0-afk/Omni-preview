# Contributing to Omni

Thanks for your interest. Omni is a Rust-first messenger with a WinUI 3 shell on Windows. This page
covers how the project is put together and the few rules it keeps. Build and run instructions are in
[DEVELOPING.md](DEVELOPING.md), the architecture is in [ARCHITECTURE.md](ARCHITECTURE.md), and the
threat model is in [omni-threat-model.md](omni-threat-model.md).

Please report security problems privately, as described in [SECURITY.md](SECURITY.md), not in a
public issue.

## How the code is split

The Rust core is the source of truth. The C# shell only renders state it gets from the core and sends
commands back over a C ABI. A feature usually lands in this order:

1. domain types (`omni-identity`, `omni-protocol`)
2. storage (`omni-storage`)
3. transport (`omni-transport`, `omni-network`)
4. orchestration (`omni-app`)
5. FFI (`omni-ffi`), only if the shell really needs it
6. the shell (`windows-app/`)
7. docs and tests in the same change

`docs/architecture/code-map.md`, `ffi-surface.md` and `storage-schema.md` are generated maps of
symbols, FFI exports and database tables. Regenerate them after a refactor with
`scripts/regen-docs.ps1`.

## Development stage

Omni is pre-1.0. There are no external users to keep compatible with, so:

- Old formats and APIs are not kept around "just in case". When something replaces an old path, the
  old one is removed, with its tests and UI.
- The UI keeps up with the core. A change to the core or FFI is carried through to the shell in the
  same change, so there are no buttons that lead nowhere.
- Data formats may change between preview releases without migration.

## Invariants

These hold everywhere; a change that breaks one needs a discussion first.

- Discovery, identity, relationship and presence are separate states. Seeing a peer on the network
  never makes it a trusted contact.
- 1:1 chat and calls are only for approved contacts. Group chat and calls are for group members;
  group membership never opens a 1:1 channel.
- One call per profile, enforced in the core.
- Ratchet sessions: read the session under `session_lock` and store it with `store_session_record`
  (optimistic, by `revision`) before sending the frame.
- Media internals do not cross the FFI boundary, and raw media is never stored as chat history.
- Everything needed to connect (pairing, adding contacts and relays) is available in the GUI. The
  daemon and CLI are for development only.
- Tests use the real network path; nothing is tunnelled through VPNs or SSH to make NAT traversal
  look easier than it is.
- No node is privileged. The app must work with an empty bootstrap list, and every relay can be
  removed by the user.

## Static registries

Some runtime state lives in process-wide registries keyed by the database path. Every registry must be
cleared in `stop_runtime_context` when a profile stops; `clear_runtime_chat_sessions_for_active_profile`
is the pattern to follow. A new registry without a cleanup path will not be merged.

## Android readiness

The Rust core is meant to be shared across platforms, with a separate UI per platform. Platform code
goes behind `#[cfg(...)]` with a fail-closed branch for platforms that are not implemented yet (see
`omni-crypto/src/crypto_local_protection.rs`). The core does not depend on Windows crates; Windows APIs
are called through raw `#[link_name]` declarations under `#[cfg(windows)]`. New scripts should come
with a `.sh` counterpart. Details: `docs/architecture/core-portability-audit.md`.

## Tests

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\test-core.ps1
dotnet build .\windows-app\Omni.Windows.sln -p:Platform=x64
```

Tests live next to the code: `tests.rs` in each crate, `windows-app/tests/`, and the native smoke tests
in `native/omni-webrtc-media/tests/`. Test names describe behaviour
(`feature_does_thing_when_condition`). A test that cannot check its behaviour on the current machine
should not pass silently: it either sets up the connection itself (loopback) or is marked `#[ignore]`
with a reason.

## Pull requests and issues

- Keep a pull request to one change, with its tests and docs.
- `cargo clippy --workspace --all-targets -- -D warnings` and the test scripts above should pass.
- If you find something wrong while working (a test that locks in a bug, dead code, a misleading
  name, an outdated doc), say so in the issue or pull request rather than building on top of it.

References such as `docs/decisions/…`, `docs/bugs/…` and `docs/notes/…` in code comments point to
internal design notes that are not part of this repository.
