---
status: audit (read-only); portability blockers of the Rust core (shared core, per-platform UI)
scope: core-rust/crates/* (omni-crypto, omni-storage, omni-app, omni-transport, omni-net-primitives, …)
policy: the core is shared across platforms; platform-specific code only behind #[cfg] + fail-closed
related:
  - docs/decisions/video-render-architecture.md (media layer, handled separately, native)
  - docs/architecture/cross-platform-media-audit.md
  - CONTRIBUTING.md §"Android readiness"
method: grep+read over core-rust; grounded in file:line
---

# Rust Core Portability Audit

Goal: the core is intended to be shared (UI per platform), so the whole core is checked for
portability blockers (primarily for Android).

**Result: the core is portable.** The platform seams already exist, and are mostly
**real** and **fail-closed**. The core's `Cargo.toml` files have **zero** Windows dependencies.
The remaining Android work is **host-side** (JNI/Keystore/AAudio/Camera), **not** core changes.

---


## Link cost (metered): the seam is ready, the Android host part is NOT written

The congestion controller is chosen by a single criterion: **whether traffic is metered on this link**
(BBR everywhere except metered; both measurements are in the doc block of `select_congestion_control`). The core
cannot learn this itself: on Windows WinRT provides it, on Android `ConnectivityManager`.

- **The seam is ready and platform-neutral:** `omni_transport_set_link_cost("metered"|"unmetered"|
  "unknown")` is an ordinary C ABI, so the JNI wrapper is thin. The core only accepts the fact and does not invent it.
- **The Windows part exists:** `LinkCostProbe` (WinRT `GetConnectionCost`; roaming and an exceeded
  data limit count as metered even on an "unlimited" type).
- **The Android part does NOT exist.** Until it does, a fail-closed branch applies: on `target_os = "android"`
  an unknown cost resolves to **Cubic**, not BBR. The reason is exactly why the
  rule exists: on a phone an unidentified link is very often cellular, and a measurement
  showed that BBR against an operator policer burns 32-38 % of traffic, which is the user's money.
  Both branches of the fork are covered by a test (`unknown_cost_choice`), so the rule for
  Android is verified even before an Android build exists.
- **What to write on Android:** read
  `ConnectivityManager.getNetworkCapabilities(activeNetwork).hasCapability(NET_CAPABILITY_NOT_METERED)`
  (plus roaming), and call the export before `mesh_start` and on network change. Without it, a phone's unlimited
  home Wi-Fi runs on Cubic: a loss of speed, not of money; the order of cost of the error
  was chosen deliberately.

## Full list of platform points in the core (this is the entire surface)

| # | Where | What | Portability status |
|---|---|---|---|
| C1 | `omni-crypto/crypto_local_protection.rs` | selector of the secret-protection scheme by OS | ✅ **ready**: win→DPAPI, android→PlatformWrappedV1, else→Plaintext (`:1-7`) |
| C2 | `omni-crypto/internals_secret_protection.rs` | DPAPI (win) + `platform_wrap` AES-256-GCM | ✅ the android wrap is real (AES-GCM, `:27-67`); the key is host-injected/env (`:69-82`) |
| C3 | `omni-crypto/crypto_local_protection.rs:62-66` | plaintext fallback | ✅ **fail-closed**: on android plaintext is **forbidden** (error), not silently insecure |
| S1 | `omni-storage/lib.rs:181-223` | system-path blocklist + normalization | ✅ full win (`\Windows`…) and unix (`/etc /usr`…) branches, `\\?\`+drive, `MAIN_SEPARATOR` |
| S2 | `omni-storage/storage_attachments.rs:1831-1940` | free-space preflight + file pre-allocation | ✅ `not(windows)` = deliberate graceful fallback: free-space→`None`, pre-alloc→no-op (these are Windows-only **optimizations**, the feature works without them; optionally `statvfs`/`fallocate` later) |
| A1 | `omni-app/lib.rs:813` | path case for comparison | ✅ lowercase **only** under `#[cfg(windows)]`; on non-win case is preserved (correct for Android/Linux) |
| N1 | `omni-net-primitives/multicast.rs:51` | multicast socket opts | ✅ setup is cross-platform via `socket2`; `#[cfg(unix)] set_reuse_port` is a correct unix-only extra (on Windows `SO_REUSEADDR` suffices), no paired win branch is needed |
| D1 | `*/Cargo.toml` | dependencies | ✅ **0** Windows crates (no winapi/windows-sys/…); DPAPI is a raw `#[link_name]` under cfg |

Tests with `#[cfg(windows)]` (omni-crypto/omni-storage/omni-app tests) are not blockers (they are simply
skipped on non-win).

---

## What this means for Android (host-side, NOT the core)
The core is ready to accept a platform; the Android work lives in the **shell/binding**, not in the crates:

1. **Keystore → wrap key.** The Android host obtains a hardware-backed key from the Keystore and
   supplies it to the core via `configure_platform_secret_wrap_key_hex` (FFI/JNI). Without a key the
   core is **fail-closed** (`PLATFORM_SECRET_WRAP_KEY_ENV must be set`). This is the only thing
   the core needs to run safely on Android.
2. **FFI → JNI.** The C ABI exists; JNI is a thin wrapper (CONTRIBUTING.md plans it this way).
3. **Media** (capture/encode/decode/render, audio) is a separate track, the native layer; see
   `cross-platform-media-audit.md` (the seam is not yet extracted there; it is the main Android work for media).
4. **Scripts** need `.sh` counterparts of the `.ps1` ones (already noted in CONTRIBUTING.md).

## Verification checklist (closed)
- **S2 → ✅**: the `not(windows)` branches are a deliberate graceful fallback of two Windows-only
  optimizations (free-space preflight → `Ok(None)`, pre-allocation → `Ok(())`). Attachment download
  on Linux/Android works without them. Not a no-op bug. Optional upgrade: `statvfs`/`fallocate`.
- **N1 → ✅**: the multicast setup is cross-platform via `socket2`; the only platform line
  `#[cfg(unix)] set_reuse_port` is correct (Windows has no `SO_REUSEPORT`; `SO_REUSEADDR`
  suffices). A paired win branch is not required.

**Zero portability blockers in the core. All points were confirmed by reading the code.**

## Conclusion
"The core is shared, with minor adjustments" is **realistic and already nearly the case**. The discipline
going forward (recorded in CONTRIBUTING.md): any new platform-specific code goes **only behind
`#[cfg(...)]`, with a fail-closed branch** on an unimplemented target (as done in crypto), and
**no Windows crates** in the core. There are no portability blockers in the core.
