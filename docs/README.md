# Omni documentation

| What do you want to do? | Start here |
|---|---|
| Install Omni for Windows | [Download, verify and run Omni](../README.md#install) |
| Share a folder, save a version or give an agent a task | [Working in a workspace](workspaces.md) |
| Build, run or debug from source | [Development guide](../DEVELOPING.md) and [contributing guidelines](../CONTRIBUTING.md) |
| Understand how the Windows app and Rust core fit together | [Architecture](../ARCHITECTURE.md) |
| Understand security guarantees or report a vulnerability | [Threat model](../omni-threat-model.md) and [security reporting](../SECURITY.md) |

## Implementation references

| Topic | Documents |
|---|---|
| Find a symbol, SQLite column or exported function | [Code map](architecture/code-map.md), [storage schema](architecture/storage-schema.md), [FFI surface](architecture/ffi-surface.md) |
| Trace a runtime operation | [Lifecycle paths](architecture/lifecycle-paths.md), [network behavior](architecture/network-behavior-map.md) |
| Networking and calls | [Mesh overlay](architecture/networking-overlay.md), [realtime media over QUIC](architecture/realtime-media-quic.md), [native media component](../native/omni-webrtc-media/README.md) |
| Windows UI and its Rust bridge | [WinUI shell](architecture/winui-shell.md), [FFI model](architecture/ffi-model.md) |
| Visual assets | [Logo files](design/logo/README.md) |

The code map, storage schema and FFI surface are generated from source. After a refactor, run `powershell -ExecutionPolicy Bypass -File scripts/regen-docs.ps1` from the repository root.

## Design history and planning

- [Product vision and scope](architecture/omni-product-vision.md)
- [Group chat and calls plan](architecture/group-chat-and-calls-plan.md)
- Portability assessments for the [core](architecture/core-portability-audit.md) and [media](architecture/cross-platform-media-audit.md)
- Founding decisions on [Rust ownership](adr/0001-rust-core-as-source-of-truth.md) and the [C ABI](adr/0002-c-abi-boundary.md)

Older references to `docs/decisions/`, `docs/bugs/` and `docs/notes/` refer to internal development notes.
