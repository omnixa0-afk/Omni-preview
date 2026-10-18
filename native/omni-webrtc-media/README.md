# Omni WebRTC Media Native Boundary

This module is the native boundary for Omni realtime camera media.

Target production path:

`WinUI camera controls/render surface -> omni_webrtc_media -> mature WebRTC media modules -> RTP over Omni QUIC DATAGRAM -> RTCP/control over reliable Omni control -> omni_webrtc_media decode -> WinUI render`

Current state:

- The C ABI and build/test harness exist.
- The module can optionally link the external WebRTC checkout and run a real builtin video codec factory probe.
- Default builds without external dependencies still report `available=false` and `backend=none`.
- WebRTC-linked builds report `backend=libwebrtc`, `codec_factory_probe_available=true`, the real software codec intersection discovered from WebRTC factories, and per-codec encode/decode color-bars smoke diagnostics.
- WebRTC-linked builds report `available=true`, `supports_rtp_over_omni_quic=true`, and `supports_rtcp_feedback=true` when at least one bounded native session codec is linked and smoke-tested: `session_create`, I420/NV12/BGRA frame input, WebRTC RTP packetizer/assembler, native RTP pacing, native RTP retransmission cache, native RTCP/control events, native jitter/playout delay, native ABR decisions, RTP-sized binary packet drain/push, decoded I420/BGRA frame drain, and per-session diagnostics through encode -> RTP -> decode.
- VP8 remains the conservative fallback. H.264 is production-eligible only when the Windows Media Foundation hardware encoder creates successfully and a bounded native smoke proves hardware encode -> WebRTC RTP packetization/session -> libwebrtc H.264 decode -> color-bars output. Software OpenH264 smoke remains diagnostic-only and must not be advertised as hardware acceleration. The probe populates `hardware_codecs=["h264"]` only after that production path smoke passes.
- Native ABR owns the active-path bitrate/FPS/resolution target and considers receiver loss, RTT trend, jitter, outgoing pacing queue pressure, queue drops, local processing, and recovery hysteresis. Diagnostics include the current ABR reason, bounded reason history, and EWMA values for loss/RTT/jitter/queue.
- Default builds without the WebRTC backend still return `OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED` for session creation.
- The Windows app copies `omni_webrtc_media.dll` into the app output when a native build exists and selects the native RTP-over-Omni-QUIC path only for codecs reported by `native_session_scaffold_codecs`.
- `scripts/bootstrap-libwebrtc.ps1` prepares external depot_tools under `D:\OmniDeps` and can perform the large WebRTC checkout outside the repository when run with `-Sync`.
- `scripts/build-libwebrtc.ps1` builds the external `webrtc.lib` plus the builtin encoder/decoder factory objects used by the probe, with proprietary H.264 and Chrome FFmpeg branding enabled for local H.264 decode proof.

Rules:

- Do not implement camera video with JSON/base64/JPEG/raw-frame shortcuts.
- Do not claim hardware codec support without native create/encode/decode proof.
- Keep RTP media on Omni QUIC DATAGRAM and RTCP/control on reliable Omni control.
- Keep queues bounded by packets and bytes.

## Build

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\build-omni-webrtc-media.ps1 -Configuration Release
```

With the external WebRTC checkout already bootstrapped and built:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\build-libwebrtc.ps1
powershell -ExecutionPolicy Bypass -File .\scripts\build-omni-webrtc-media.ps1 -Configuration Release -UseLibWebRtc
```

## External WebRTC Source Bootstrap

The full WebRTC checkout is intentionally outside the repository:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\bootstrap-libwebrtc.ps1
powershell -ExecutionPolicy Bypass -File .\scripts\bootstrap-libwebrtc.ps1 -Sync
```

Do not commit depot_tools, the WebRTC source checkout, generated build files, or compiled binaries.
