# Omni docs — index

"Which question → which document." One home per topic; if you are about to write something that
explains an existing thing, update its document instead.

## Start here
| You want… | Read |
|---|---|
| Build, run, debug | `../DEVELOPING.md` |
| Project rules, invariants, where things live | `../CONTRIBUTING.md` |
| Architecture overview | `../ARCHITECTURE.md`, `architecture/overview.md` |
| Product vision & scope (what we build / anti-features) | `architecture/omni-product-vision.md` |
| Threat model | `../omni-threat-model.md` |

## Find code / structure
| You want… | Read | Note |
|---|---|---|
| Symbol → file (Rust/C#/native) | `architecture/code-map.md` | auto-gen |
| SQLite tables & columns | `architecture/storage-schema.md` | auto-gen |
| FFI exports (`omni_*`) by domain | `architecture/ffi-surface.md` | auto-gen |
| Hot-path chains (send / call-end / queue tick / transport cb) | `architecture/lifecycle-paths.md` | manual |
| WinUI shell structure | `architecture/winui-shell.md` | manual |

Regenerate the auto-gen maps after refactors/new FFI/migrations:
`powershell -ExecutionPolicy Bypass -File scripts/regen-docs.ps1`.

## Architecture & design
| Topic | Doc |
|---|---|
| How the network behaves at runtime (start → carriers → discovery → rendezvous → workers → degradation) | `architecture/network-behavior-map.md` |
| Mesh overlay spec | `architecture/networking-overlay.md` |
| Realtime media over QUIC | `architecture/realtime-media-quic.md`, `../native/omni-webrtc-media/README.md` |
| FFI model | `architecture/ffi-model.md` |
| Groups and group calls plan | `architecture/group-chat-and-calls-plan.md` |
| Android / cross-platform readiness | `architecture/core-portability-audit.md`, `architecture/cross-platform-media-audit.md` |
| Logo | `design/logo/` |
| Founding architecture decisions (Rust core as the source of truth, C ABI boundary) | `adr/` |

## Notes
- References to `docs/decisions/…`, `docs/bugs/…` and `docs/notes/…` in code and in older documents point to
  internal design notes that are not part of this repository.
- ADRs are append-mostly: supersede (and say so), don't silently delete.
