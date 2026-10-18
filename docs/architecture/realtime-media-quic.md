# Realtime Media Over QUIC

Production video is V2-only:

`WinUI camera capture -> native omni-webrtc-media/libwebrtc -> RTP over QUIC DATAGRAM -> RTCP/control over reliable Omni control -> native decode -> WinUI render`

Forbidden paths:

- JSON/base64 media frames
- request-response media packets
- JPEG/PNG frame streaming
- reliable streams for realtime RTP media payloads
- fake active call state when V2 media transport is unavailable

Current transport status:

- Omni uses an owned Quinn QUIC carrier for the secure session.
- The carrier exposes session-bound QUIC DATAGRAM send/receive for RTP-sized media packets.
- `omni-app` owns a bounded per-dialog inbound DATAGRAM queue and `omni-ffi` exposes a binary packet pump for native media. Media packets do not cross JSON/base64.
- Reliable QUIC streams remain separate for control, attachment transfer, encrypted realtime media capability negotiation, and RTCP/session feedback.
- `native/omni-webrtc-media` exists as the native C ABI boundary and smoke-test harness. Default builds have no external media backend. WebRTC-linked builds run a real builtin encoder/decoder factory probe and bounded native session smoke through I420/NV12/BGRA input -> WebRTC RTP packetizer/assembler -> native pacing/control/playout -> decoded I420/BGRA output, then report `available=true`/`supports_rtp_over_omni_quic=true`/`supports_rtcp_feedback=true` for the codecs that pass that proof.
- Realtime media capability negotiation is Rust-owned. WinUI publishes local codec/capture capabilities, Rust exchanges encrypted logical events over the active chat session, chooses a common codec, capture pixel format, bitrate, resolution, frame rate, RTCP-feedback mode, and DATAGRAM size, then WinUI applies that exact selection.
- The Windows call surface exposes codec, resolution, and FPS preferences. `Auto` publishes a conservative probe-confirmed profile; explicit resolution or FPS choices publish that axis as `min=preferred=max` so Rust either selects the requested profile on both peers or fails visibly instead of silently downgrading.
- The active Windows media path uses the native RTP-over-Omni-QUIC session for probe-confirmed codecs. H.264 is backed by a Windows Media Foundation hardware encoder and the existing WebRTC RTP/RTCP/decode modules only when native smoke proves hardware encode -> RTP session -> decode -> color-bars output. Software OpenH264 smoke is diagnostic-only: it must not be advertised in production capabilities, and forced H.264 must fail visibly when native hardware proof is absent.
- Realtime media selection is Rust-owned but respects the deterministic mutual preference order published by both peers. NV12/I420 are preferred for the native media path; BGRA remains a fallback for cameras that cannot provide a native-friendly raw format. If a negotiated capture pixel format cannot be opened, video fails visibly instead of silently switching formats.
- Explicit user-selected resolution/FPS profiles are enforced again at capture open time. If the selected camera cannot provide at least the requested resolution or frame rate, sender startup fails with `capture_unavailable` instead of quietly sending a lower profile.
- The active Windows capture path publishes NV12 ahead of BGRA and opens WinRT MediaCapture first. OpenCV/BGRA is retained only as a fallback for cameras or drivers that cannot satisfy the negotiated profile through WinRT. Same-device two-window tests should report camera contention for the second local sender instead of masking the limitation as network loss.
- `Auto` capture may adapt after live warm-up when the driver advertises a profile but materially under-delivers FPS at runtime. The sender first reports `capture_throttled`, then restarts only the local WinRT reader on the next lower same-pixel-format profile. QUIC, native RTP sessions, RTCP/control, and the dialog stay alive; explicit user-selected resolution still fails visibly instead of silently downshifting.
- ABR downscale preserves NV12 for NV12 input; it no longer converts the encode hot path to BGRA just because resolution changed.
- The native sender packetizes and drains RTP packets through a bitrate-aware pacer before C# sends QUIC DATAGRAM batches. Native keeps the retransmission cache for NACK, owns ABR decisions from receiver/local-processing samples, tracks loss/RTT/jitter/queue EWMA plus reason history, and keeps negotiated caps separate from the current adaptive encode size so downscale can recover without losing the original profile.
- The active Windows media path uses only native RTP-sized QUIC DATAGRAM batches. Managed RTP packetization/playout and legacy call-media JSON payloads are not production fallbacks.
- Video acceptance must check more than packet counters: color reproduction, negotiated resolution, bitrate, sender FPS, receiver/rendered FPS, codec hardware/software marker, native ABR reason history, queue drops, decode/render stalls, and whether counters keep growing without freezes.

Required next media hardening:

1. Evolve the native ABR controller from the current pragmatic loss/jitter/RTT/queue EWMA policy toward a mature WebRTC-grade congestion controller without enabling full PeerConnection, ICE, DTLS, or SCTP.
2. Add native hardware decoder proof where available. Current production H.264 proof is hardware encode plus libwebrtc decode; the probe must keep reporting the exact encoder/decode split.
3. Extend the local camera probe into the native media boundary so source-specific hardware capture/codec proof is owned by the same layer that encodes.
4. Extend the existing live soak automation for IPv6, IPv4 STUN, forced codec, camera contention, packet-loss/jitter cases, and freeze-counter checks.
5. Keep H.264 hardware support gated by real create/encode/RTP/decode smoke; forced H.264 must fail visibly when unsupported.

If the V2 QUIC DATAGRAM carrier is unavailable, calls fail visibly with `VIDEO_TRANSPORT_V2_REQUIRED` instead of falling back to legacy media.
