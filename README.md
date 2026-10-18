# Omni

Omni is a messenger for Windows for chatting and calling with friends and family. Devices connect to
each other directly, and messages, calls and files are end-to-end encrypted. Your account is a
cryptographic key, so no phone number or email is needed.

> **Preview.** Omni is pre-1.0 software under active development. It has not had an independent
> security audit, data formats may change between preview releases without migration, and things will
> break. Please don't rely on it for anything important yet.

## What it does

- 1:1 encrypted chat with queue-first delivery (messages wait and go out when the peer is reachable)
- 1:1 voice and video calls, screen sharing
- file and photo transfer
- group chats and group calls, with files shared among members
- direct connections between devices (UDP or TCP, with NAT traversal), and optional relays you add
  yourself when a direct link is not possible
- finding contacts through existing public services (Nostr relays, ntfy, the BitTorrent DHT) using
  encrypted announcements
- a recovery phrase to restore your account

Identity, contact relationship, verification and presence are kept as separate states: seeing a peer
on the network never makes it a trusted contact. 1:1 chat and calls are open only to contacts you approved,
group chat only to members of the group.

### Privacy

Messages, calls and files are end-to-end encrypted. Omni does not hide your IP address: peers you
connect to directly can see it. Details are in [omni-threat-model.md](omni-threat-model.md).

## Install

1. Download `Omni.exe` from [Releases](../../releases).
2. Check its SHA-256 against `SHA256SUMS.txt` in the same release:
   ```powershell
   Get-FileHash .\Omni.exe -Algorithm SHA256
   ```
3. Run it. It is a single portable file: no installer, no .NET or runtime installs. Profile data is
   kept in an `OmniData\` folder next to the exe.

The exe is not code-signed, so Windows SmartScreen will warn on first launch
(**More info → Run anyway**). Releases are built from the tagged source in this repository with
`scripts/release.ps1`, and you can build from the same tag yourself (the build is not bit-for-bit
reproducible yet, so your exe's hash will differ).

Requirements: Windows 10 1809+ or Windows 11, x64.

## Build from source

You need the .NET 10 SDK, Rust (stable, MSVC toolchain) and the Visual Studio C++ build tools.

```powershell
dotnet publish windows-app/src/Omni.Windows.App/Omni.Windows.App.csproj -p:PublishProfile=win-x64-standalone
```

The WinUI project builds the Rust core itself. Calls (audio and video) come from the native
media module in `native/`, which is built separately; without it the app has chat and files but no
calls. Full setup, debugging and troubleshooting: [DEVELOPING.md](DEVELOPING.md).

## Repository

| Path | What |
|---|---|
| `core-rust/` | Rust core: protocol, crypto, identity, storage, mesh network, rendezvous. The source of truth |
| `windows-app/` | WinUI 3 shell: renders state from the core and sends commands over a C ABI |
| `native/omni-webrtc-media/` | C++ media engine (capture, codecs, RTCP) |
| `docs/` | architecture notes and ADRs |

Start with [ARCHITECTURE.md](ARCHITECTURE.md), then [docs/README.md](docs/README.md).

## Contributing and security

- How the project is organised and the rules it keeps: [CONTRIBUTING.md](CONTRIBUTING.md)
- Found a vulnerability? Please follow [SECURITY.md](SECURITY.md) instead of opening a public issue.

## License

Omni is free software under the [GNU Affero General Public License v3.0](LICENSE). If you distribute
a modified version, or let others use one over a network, you must offer its source under the same
license. The software comes with no warranty (see the license).
