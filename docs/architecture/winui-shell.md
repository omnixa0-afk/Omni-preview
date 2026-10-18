# WinUI Shell

This document describes the current WinUI 3 shell shape as it exists in the repository today.

## Current Structure

The Windows client is still a single-window app, but the shell is no longer one giant XAML file with all sections inline.

- `windows-app/src/Omni.Windows.App/MainWindow.xaml`
  - shell composition only
- `windows-app/src/Omni.Windows.App/MainWindow.xaml.cs`
  - window lifecycle and shell orchestration only
- section controls:
  - `Features/Onboarding/OnboardingShellControl.xaml`
  - `Features/Chats/ChatsSectionControl.xaml`
  - `Features/Contacts/ContactsSectionControl.xaml`
  - `Features/Devices/DevicesSectionControl.xaml`
  - `Features/Files/FilesSectionControl.xaml`
  - `Features/Settings/SettingsSectionControl.xaml`
  - `Features/Diagnostics/DiagnosticsSectionControl.xaml`
- support feature modules:
  - `Features/Calls/`
    - call presentation helpers and future realtime media UI
  - `Features/LanPeers/`
    - LAN peer presentation helpers
  - `Features/Profiles/`
    - saved-profile presentation helpers

Most feature logic lives in partial files and small helpers instead of one monolithic `MainWindow` body.

## Refresh Model

The shell does not use the old fixed `250 ms` UI pump.

Current behavior:

- one authenticated refresh loop computes the nearest due time across visible domains
- the loop sleeps until that deadline instead of waking the UI at a fixed cadence
- important actions can wake the loop early:
  - active dialog changes
  - selected contact or selected device changes
  - call state transitions
  - transport or discovery state changes
  - message, attachment, and diagnostics actions finish
- background refresh fetches domains separately and applies partial results as soon as they are ready
- UI-thread work is kept to state application; background fetch stays off the UI thread
- realtime media must use the V2 native/QUIC media path, not the general refresh loop

## Visibility-Aware Cadence

The shell now treats hidden sections as wake-only instead of polling them continuously.

Visible cadence rules:

- chats:
  - chat header `500 ms`
  - chat thread list `500 ms`
  - messages `500 ms`
  - queue diagnostics `1 s`
- calls:
  - call inbox / current call `500 ms` while `Ringing`, `Connecting`, `Active`, or `Reconnecting`
  - wake-only while idle
- contacts surfaces:
  - transport `1 s`
  - discovery `1 s`
  - contacts list `1 s`
- diagnostics surfaces:
  - transport `1 s`
  - discovery `1 s`
  - queue diagnostics `1 s`
  - devices and footprint `2 s`
- settings surfaces:
  - transport and discovery widgets `1 s`
  - settings/password/footprint widgets `2 s`
- devices surfaces:
  - devices and footprint `2 s`
- lifecycle:
  - `2 s`

Hidden transport, discovery, contacts, queue, devices, settings, and footprint domains do not refresh on elapsed cadence alone. They refresh only when:

- explicitly woken by a domain event
- or a full forced refresh is requested

The `5 s` idle heartbeat is only a loop wake/coalescing safety net. It does not mean hidden domains are polled every five seconds.

## Composite Snapshot Path

The shell no longer assembles fast transport and discovery screens from many unrelated FFI calls.

Current coarse snapshot endpoints already in use include:

- `GetLifecycleFastSnapshot()`
- `GetChatHeaderScreenSnapshot(dialogKey)`
- `GetCallInboxSnapshot()`
- `GetContactsScreenSnapshot()`
- `GetLinkedDevicesScreenSnapshot()`
- `GetDevicesDiagnosticsBundle()`
- `GetDialogMessageDelta(dialogKey, cursor)`
- `GetTransportDiscoveryRefreshPayload()`

`GetTransportDiscoveryRefreshPayload()` is the main background path for:

- transport snapshot
- discovery snapshot
- queue diagnostics
- direct-route diagnostics
- sync-route summary

Foreground refresh helpers prefer cached composite snapshots before falling back to standalone FFI reads.

## Manual Network Runtime

Configured transport and discovery settings are preserved, but they do not auto-start when the shell enters authenticated mode.

Current rule:

- entering an authenticated profile does not start listener or discovery automatically
- background refresh only reads runtime/config state
- listener and discovery start only after explicit user action in the current session
- the WinUI project runs unpackaged by default through `WindowsPackageType=None`
- packaged media capabilities are declared in `Package.appxmanifest` as native media surfaces are enabled
- unpackaged device permission must be handled honestly through runtime device initialization and error reporting

The UI intentionally distinguishes:

- configured mode
- current runtime state
- configured-but-not-started state

## Current Shell Responsibilities

The shell is responsible for:

- rendering projections
- capturing user intent
- scheduling refreshes
- caching recent snapshots for smoother foreground refresh
- rendering call and realtime media projections
- handling local media permission and default-device changes

The shell is not responsible for:

- transport truth
- trust or approval decisions
- cryptographic identity checks
- direct manipulation of SQLite state
- call eligibility, call outcomes, or media-key truth

## Performance Notes

Work that already landed:

- deadline-driven refresh scheduler instead of fixed wake-ups
- dispatcher single-flight guard to avoid queue flooding
- partial apply for background refresh domains
- composite transport/discovery/queue routing payloads
- in-memory caching for discovery network inventory
- contacts and linked devices refresh through Rust-side screen snapshots
- selected contact and selected device detail panes revision-gated before full detail fetch
- active chat keeps a full first load and then refreshes through Rust-side deltas
- call state uses Rust-side snapshots; realtime media must stay out of the general shell refresh loop

Intentionally conservative parts that still remain:

- the first load of a dialog is still a full history read for UX stability
- `current-safe` remains the only shipping default publish path
- `lean-release` and `dynamic-crt-experiment` remain measurement-only
- the shipped call surface is still 1:1 only, even though the call contracts are participant-based

This is intentional: the shell is faster and quieter than the old baseline without changing the product boundary or shipping defaults.
