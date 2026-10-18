---
status: audit (read-only); conformance to the vendor-neutral / cross-platform / zero-copy policy
scope: media and platform layers (native/omni-webrtc-media, windows-app media, build)
policy: docs/decisions/video-render-architecture.md
method: grep+read over the code; grounded in file:line
---

# Audit: Media Stack Conformance to the "Vendor-Neutral + Cross-Platform + Zero-Copy" Policy

The check: is anything in the messenger tied to a specific
GPU/vendor, or does anything hinder an efficient Android port. The policy is in the ADR
`video-render-architecture.md`.

**Result:** **PASS** on **vendor neutrality** (no ties at all). On
**cross-platform and zero-copy there are 6 gaps**, almost all of them architectural debt for the
Android iteration plus the redesign of video reception (V1). Nothing urgent on desktop
except receive efficiency.

---

## ✅ Conforms (leave as is)

| # | What | Evidence |
|---|---|---|
| P1 | **HW encode is vendor-neutral**: enumerates any IHV MFT, does not hardcode NVIDIA | `omni_webrtc_media.cpp:2131` `MFTEnumEx(MFT_ENUM_FLAG_HARDWARE \| SORTANDFILTER)` → takes the first candidate; falls back to libwebrtc |
| P2 | **No vendor SDKs anywhere**: no NVENC/NVDEC/CUDA, no AMF, no QSV/oneVPL | grep over the whole repo: 0 matches (the only "NVIDIA" is in a doc, describing which encoder was picked on one test machine) |
| P3 | **Codecs are open/SW**: H264 (OpenH264), VP9, AV1 (dav1d), Opus | CMake: `WEBRTC_USE_H264`, `RTC_ENABLE_VP9`, `RTC_DAV1D_IN_INTERNAL_DECODER_FACTORY`; no IHV codec libs |
| P4 | **Platform code under `#if defined(_WIN32)`**: does not leak unconditionally | `omni_webrtc_media.cpp:29,1063,2861,…`, all Win blocks are gated |
| P5 | **A neutral GPU stack is already linked**: the means for V1 are in place | CMake:228-235 `d3d11.lib dxgi.lib mf.lib mfplat.lib mfuuid.lib` |

Conclusion on the worrying question (vendor lock): **clean**. Encode is already a model of how it should be
(OS abstraction + SW fallback).

---

## ⚠️ Gaps vs the policy

### V1 — [EFFICIENCY, now + Android] Video receive: 3× CPU + pixels across FFI
Decode is **software** (libwebrtc `builtin_video_decoder_factory`, I420) →
I420→BGRA via **libyuv (CPU)** → the BGRA buffer **crosses FFI** → `WriteableBitmap`
blit on the UI thread.
- Evidence: `omni_webrtc_media.cpp:57` builtin decoder factory, `:65-68` libyuv
  convert, `NativeWebRtcDecodedBgraFrame` → `CallVideoService.cs:2937-2956` drain BGRA,
  `:3185-3194` `RenderFrameToImage`→WriteableBitmap.
- **Violates** the invariant "media internals do not cross FFI" and zero-copy (ADR §3).
- **Asymmetry**: encode is hardware (MF), decode is SW. On Android CPU decode + convert + blit means
  battery drain and heat.
- Fix: HW decode (DXVA/MediaCodec) + rendering to a GPU surface, so the frame never goes through CPU/FFI.

### V2 — [CROSS-PLATFORM] A monolith without an interface seam
The whole media layer is one `src/omni_webrtc_media.cpp` (~6700 lines), everything inline under
`#if _WIN32`; there is no `__ANDROID__` branch. On the NDK it compiles to `available:false,backend:none`
(`:153`).
- Evidence: CMake:13-15 a single source; grep `__ANDROID__` in native → 0.
- This is "disabled", not "portable". Fix: interfaces `IVideoRenderer/IVideoDecoder/
  IAudioDevice` + backends (ADR §4). Until then "can build for the Android NDK" (CONTRIBUTING.md)
  is aspirational.

### V3 — [CROSS-PLATFORM] Audio is WASAPI only, no seam
`OmniWasapiCapture/Render` use `audioclient.h`/`mmdeviceapi.h` directly under `_WIN32`
(`:43-44,1155,1334`). Android needs AAudio behind `IAudioDevice`. The same monolith barrier as V2.

### V4 — [CROSS-PLATFORM/BUILD] libyuv ARM SIMD is disabled
CMake:181-185 `LIBYUV_DISABLE_NEON/SVE/SME/LSX/LASX`. NEON is ARM (Android). Fine for now
(x64 only), but in an Android build color conversion would be scalar (slow). An x86-only
build tuning that silently de-optimizes mobile. (Ideally on Android the conversion is not
on the CPU at all; see V1.)

### V5 — [CROSS-PLATFORM] Media orchestration in the C# shell
RTP pack/unpack, frame scaling and format selection live in `CallVideoService.cs` (4061 lines),
a platform shell that Android will not reuse, so it would be rewritten rather than ported.
This goes against "Rust is the source of truth, C# only renders". A candidate for moving down into shared native/Rust.

### V6 — [BUILD, minor] No NDK config
CMake has only `WEBRTC_WIN`, `RTC_ENABLE_WIN_WGC`, `WINAPI_FAMILY_DESKTOP_APP`; no
`if(ANDROID)`/NDK toolchain/`WEBRTC_ANDROID`. Also the scripts are `.ps1` only (CONTRIBUTING.md already
notes "bash counterparts are needed").

---

## Severity / when to fix
| Gap | Impact now (desktop) | Impact on Android | When |
|---|---|---|---|
| V1 | medium (CPU, soft image) | high (battery) | the HW-decode/GPU-render redesign (soon) |
| V2 | none | portability blocker | Android iteration (together with the V1 interface work) |
| V3 | none | audio blocker | Android iteration |
| V4 | none | medium (speed) | Android build |
| V5 | low (coupling) | high (duplicate implementation) | Android iteration |
| V6 | none | needed for the build | Android build |

**Main point:** there is no vendor lock; all the work is the interface seam (ADR §4) plus zero-copy
receive (V1). Doing V1 according to the ADR closes V1 and lays the seam for V2/V3.
