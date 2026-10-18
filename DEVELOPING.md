# Developing Omni

How to build, run, test and debug Omni from source. Project rules and the code map are in
[CONTRIBUTING.md](CONTRIBUTING.md); architecture is in [ARCHITECTURE.md](ARCHITECTURE.md).
All commands run from the repository root in PowerShell.

## What the project is made of

| Part | Language | Where | Built by |
|---|---|---|---|
| Core (protocol, network, crypto, storage, logic) | Rust | `core-rust/` | Cargo |
| Shell (windows, controls, screens) | C# / WinUI 3 | `windows-app/` | dotnet / MSBuild |
| Media engine (capture, codecs) — optional | C++ | `native/` | CMake |

Rust is the source of truth; C# only renders state and sends commands over a thin C ABI. A feature's
logic goes into Rust, the shell only displays it.

You do not build Rust separately: `dotnet build` (and F5 in an IDE) runs `cargo build` itself and
places `omni_ffi.dll` next to the app. Without the C++ media DLL the app builds and runs, only without
video codecs.

## Prerequisites

- **.NET SDK 10** — pinned in `global.json` (`dotnet --list-sdks`)
- **Rust stable** via rustup; the `x86_64-pc-windows-msvc` target comes from `core-rust/rust-toolchain.toml`
- **Visual Studio 2022/2026** with the C++ and Windows App SDK workloads (XAML designer, C# debugging,
  MSVC linker for Rust)
- **CMake** — only for the native media module
- **VS Code** with rust-analyzer is convenient for the Rust side

```powershell
rustc --version; dotnet --list-sdks; cmake --version
```

## What to open

- **Visual Studio:** open `windows-app\Omni.Windows.sln` (not the repository root), choose **Debug**
  and **x64** (not Any CPU), press F5.
- **VS Code:** open the repository root; point rust-analyzer at `core-rust/Cargo.toml`
  (`"rust-analyzer.linkedProjects": ["core-rust/Cargo.toml"]`) since there is no `Cargo.toml` at the root.

## Everyday commands

```powershell
# Build the whole app (Rust + C#)
dotnet build .\windows-app\Omni.Windows.sln -p:Platform=x64

# Run it
.\windows-app\src\Omni.Windows.App\bin\x64\Debug\net10.0-windows10.0.19041.0\win-x64\Omni.Windows.App.exe

# Core tests (all) / one crate
powershell -ExecutionPolicy Bypass -File .\scripts\test-core.ps1
cargo test --manifest-path core-rust\Cargo.toml -p omni-network

# Rust only (fast iteration)
cargo build --manifest-path core-rust\Cargo.toml -p omni-ffi --target x86_64-pc-windows-msvc

# Format / lint
cargo fmt --manifest-path core-rust\Cargo.toml
cargo clippy --manifest-path core-rust\Cargo.toml --workspace

# Regenerate the code map, storage schema and FFI surface docs
powershell -ExecutionPolicy Bypass -File .\scripts\regen-docs.ps1
```

## Two instances on one machine

Give each instance its own data directory:

```powershell
$exe = ".\windows-app\src\Omni.Windows.App\bin\x64\Debug\net10.0-windows10.0.19041.0\win-x64\Omni.Windows.App.exe"
& $exe --app-dir "$env:TEMP\omni-a"
& $exe --app-dir "$env:TEMP\omni-b"
```

`scripts\two-instance-local.ps1` starts two instances of one account (a second device) to check
multi-device behaviour without a second machine.

## Release build (single portable exe)

```powershell
dotnet publish windows-app/src/Omni.Windows.App/Omni.Windows.App.csproj -p:PublishProfile=win-x64-standalone
```

The result is one self-contained `Omni.Windows.App.exe` in
`windows-app\src\Omni.Windows.App\bin\x64\publish-standalone\` that runs on a clean Windows 10/11 x64
without installing anything. Release builds remap source paths, so the binaries do not record where
they were built.

### Native media module (video codecs)

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\bootstrap-libwebrtc.ps1 -Sync   # depot_tools + libwebrtc checkout (large)
powershell -ExecutionPolicy Bypass -File .\scripts\build-libwebrtc.ps1
powershell -ExecutionPolicy Bypass -File .\scripts\build-omni-webrtc-media.ps1 -Configuration Release -UseLibWebRtc
```

`-DependencyRoot` (and `-WebRtcSrc` for the last script) choose where the libwebrtc checkout lives;
the default is `D:\OmniDeps`. The app build picks the DLL up automatically once it exists. Details:
`native/omni-webrtc-media/README.md`.

### Publishing a release

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\release.ps1
```

Builds the portable exe, names it `Omni.exe` and writes `SHA256SUMS.txt`. Output goes to `dist\`.

Published releases are not built on a developer machine: `.github/workflows/release.yml` builds
everything from source on a GitHub-hosted runner when a `v*` tag is pushed — libwebrtc at the revision
pinned in `scripts/bootstrap-libwebrtc.ps1`, the native media module, the core and the shell — and
attaches a build provenance attestation. To check that a downloaded `Omni.exe` came from that workflow
and from which commit:

```powershell
gh attestation verify Omni.exe -R <owner>/<repo>
```

## Troubleshooting

| Symptom | Fix |
|---|---|
| Visual Studio opened the repo but there is nothing to run | Open `windows-app\Omni.Windows.sln` instead of the folder |
| `cargo: command not found` during the C# build | Rust is not on PATH for the IDE; reopen it after installing rustup |
| `Rust DLL not found ... after cargo build` | `cargo build` failed — the real Rust error is earlier in the build output |
| `omni_ffi.dll` is locked | The app is still running; close every `Omni.Windows.App.exe` |
| Platform errors | Build **x64** only, not Any CPU or x86 |
| `net10.0` is not recognised | Install the .NET 10 SDK and a current Visual Studio |
| Want a clean rebuild | Delete `core-rust\target` and `windows-app\src\Omni.Windows.App\bin` + `obj` |

## Before opening a PR

- Run the core tests and build the app.
- A core/FFI change reaches the C# shell in the same change ("UI in step with the core",
  [CONTRIBUTING.md](CONTRIBUTING.md)); regenerate the docs maps if exports or migrations changed.
- Do not commit build output, local databases, logs or screenshots — they are in `.gitignore`.
