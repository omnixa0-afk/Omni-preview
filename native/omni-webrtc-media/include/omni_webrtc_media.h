#pragma once

#include <stddef.h>
#include <stdint.h>

#if defined(_WIN32)
#if defined(OMNI_WEBRTC_MEDIA_BUILDING_DLL)
#define OMNI_WEBRTC_MEDIA_API __declspec(dllexport)
#else
#define OMNI_WEBRTC_MEDIA_API __declspec(dllimport)
#endif
#else
#define OMNI_WEBRTC_MEDIA_API __attribute__((visibility("default")))
#endif

#ifdef __cplusplus
extern "C" {
#endif

/* Also written into the probe JSON literals in omni_webrtc_media.cpp (the no-backend and the
 * factory-failure answers) — bump those together with this. */
#define OMNI_WEBRTC_MEDIA_ABI_VERSION 23u

typedef enum OmniWebRtcMediaStatus {
    OMNI_WEBRTC_MEDIA_STATUS_OK = 0,
    OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT = 1,
    OMNI_WEBRTC_MEDIA_STATUS_BUFFER_TOO_SMALL = 2,
    OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED = 3,
    OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR = 4
} OmniWebRtcMediaStatus;

typedef struct OmniWebRtcMediaSession OmniWebRtcMediaSession;

/*
 * ИСХОДЯЩИЕ ПАКЕТЫ (ABI 13, 2026-09-26). Каждый пакет, который выпускает сессия или захват звука
 * (RTP отправителя, RTCP приёмника, Opus), отдаётся сюда СРАЗУ, на том потоке, который его
 * выпустил, — как `Transport::SendRtp` у апстрима отдаёт пакет в сокет. Получатель — функция ядра
 * `omni_realtime_media_packet_sink` со своим контекстом; оболочка только связывает их при старте
 * звонка. Очереди, которую оболочка выгребала таймером, больше нет — см.
 * docs/bugs/media-rode-the-shells-timer.md.
 *
 * Функция зовётся под замком получателя: после `set_packet_sink(…, NULL, NULL)` вызовов в полёте
 * нет, и контекст можно освобождать. Пакеты, выпущенные без получателя, теряются и считаются
 * (`packets_unrouted` в диагностике).
 */
typedef void (*OmniWebRtcMediaPacketSink)(void* context, const uint8_t* packet, size_t packet_len);

/*
 * Структурных событий управления больше нет (ABI 19, 2026-10-03): NACK, PLI, RR, SR и XR — настоящий
 * RTCP модулей сессии, уходит тем же получателем пакетов, что и медиа. Оболочка в обратной связи
 * не участвует. См. docs/bugs/media-rode-the-shells-timer.md, шаги 3–5.
 */

typedef enum OmniWebRtcMediaAbrState {
    OMNI_WEBRTC_MEDIA_ABR_STATE_STABLE = 0,
    OMNI_WEBRTC_MEDIA_ABR_STATE_HOLD_CONGESTED_PROBE = 1,
    OMNI_WEBRTC_MEDIA_ABR_STATE_HOLD_MODERATE_PROBE = 2,
    OMNI_WEBRTC_MEDIA_ABR_STATE_DOWN_LOCAL_PROCESSING = 3,
    OMNI_WEBRTC_MEDIA_ABR_STATE_DOWN_CONGESTED = 4,
    OMNI_WEBRTC_MEDIA_ABR_STATE_DOWN_MODERATE = 5,
    OMNI_WEBRTC_MEDIA_ABR_STATE_UP_STABLE = 6
} OmniWebRtcMediaAbrState;

/*
 * ЧТО СЮДА КЛАСТЬ, А ЧТО НЕТ (ABI 12, #61 шаг D3).
 *
 * Отсюда убраны render_stalls, encode_ms, capture_processing_ms, sent_fps и target_fps. Они
 * описывали ЛОКАЛЬНУЮ перегрузку захвата и кодирования, и по ним ядро само решало, насколько
 * срезать частоту кадров и размер. Этой политики больше нет: перегрузку меряет
 * OveruseFrameDetector ВНУТРИ VideoStreamEncoder — по доле времени, которую занимает
 * кодирование, на том самом кодировщике, который работает, а не по пересказу оболочки через FFI.
 *
 * Живой замер 2026-08-17 (город А <-> город Б, 121 решение ABR) показал, чего стоил пересказ:
 * пороги 33-41 мс против замеренных 47-91 мс на захвате, то есть «перегружены» было истинно
 * всегда, включая 320x180, и каждый шаг разрешения вверх немедленно отменялся шагом вниз.
 *
 * Поля не оставлены «на всякий случай» намеренно: то, что переходит границу и никем не
 * читается, следующий человек считает работающим входом.
 */
typedef struct OmniWebRtcMediaAbrSample {
    /*
     * Часы такта. Всё остальное, что здесь было (потери, джиттер, RTT собеседника и давность его
     * обратной связи), с ABI 18 сессия знает сама: RR приходит отправителю видео байтами RTCP.
     * Давность по-прежнему взводит откат «отчётов не было ни одного» (молчание собеседника при
     * нашей отправке — docs/bugs/call-drowns-the-mobile-uplink.md), но считается по факту прихода
     * отчётов на этих же часах.
     */
    int64_t now_ms;
} OmniWebRtcMediaAbrSample;

typedef struct OmniWebRtcMediaAbrTarget {
    uint32_t changed;
    uint32_t target_bitrate_bps;
    uint32_t target_fps;
    uint32_t target_width;
    uint32_t target_height;
    uint32_t state;
} OmniWebRtcMediaAbrTarget;

OMNI_WEBRTC_MEDIA_API uint32_t omni_webrtc_media_abi_version(void);

OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_probe_json(
    char* buffer,
    size_t buffer_len,
    size_t* required_len);

OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_session_create(
    const char* config_json,
    OmniWebRtcMediaSession** out_session,
    char* error_buffer,
    size_t error_buffer_len,
    size_t* required_error_len);

OMNI_WEBRTC_MEDIA_API void omni_webrtc_media_session_destroy(
    OmniWebRtcMediaSession* session);

OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_session_push_i420_frame(
    OmniWebRtcMediaSession* session,
    const uint8_t* y_plane,
    int32_t y_stride,
    const uint8_t* u_plane,
    int32_t u_stride,
    const uint8_t* v_plane,
    int32_t v_stride,
    int32_t width,
    int32_t height,
    int64_t timestamp_us,
    int32_t force_keyframe);

OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_session_push_nv12_frame(
    OmniWebRtcMediaSession* session,
    const uint8_t* y_plane,
    int32_t y_stride,
    const uint8_t* uv_plane,
    int32_t uv_stride,
    int32_t width,
    int32_t height,
    int64_t timestamp_us,
    int32_t force_keyframe);

OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_session_push_bgra_frame(
    OmniWebRtcMediaSession* session,
    const uint8_t* bgra,
    int32_t bgra_stride,
    int32_t width,
    int32_t height,
    int64_t timestamp_us,
    int32_t force_keyframe);

OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_session_set_rates(
    OmniWebRtcMediaSession* session,
    int32_t bitrate_bps,
    int32_t fps);
/* ABI 23: a new bitrate ceiling on a live sender session (the negotiated target changed, e.g.
 * Settings -> Calls -> bitrate level). The congestion controller keeps its estimate; a target above
 * the new ceiling drops to it at once. No new encoder, no keyframe. */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_session_set_max_bitrate(
    OmniWebRtcMediaSession* session,
    int32_t max_bitrate_bps);

OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_session_request_keyframe(
    OmniWebRtcMediaSession* session);

OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_session_set_packet_sink(
    OmniWebRtcMediaSession* session,
    OmniWebRtcMediaPacketSink sink,
    void* context);

OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_session_push_rtp_packets(
    OmniWebRtcMediaSession* session,
    const uint8_t* buffer,
    size_t buffer_len,
    uint32_t packet_count);

/*
 * Оценка канала по фактической доставке (RFC 8888). Приёмник копит времена прибытия и отдаёт
 * готовый отчёт тем же путём, что и остальной RTCP приёмника (OmniWebRtcMediaPacketSink). Раньше
 * он ехал управляющим каналом, который оболочка опрашивала раз в 50 мс с обеих сторон, и эти два
 * периода целиком входили в измеренный медиа-RTT (замер на loopback: 87–164 мс при пути 0.1 мс).
 *
 * На принимающей стороне отчёт узнаётся по RTCP-заголовку (тип 205, формат 11) и отдаётся
 * ОТПРАВЛЯЮЩЕЙ сессии через apply_transport_feedback — приёмной он не адресован. Отдельный вход
 * нужен и после переезда: помимо байтов сборщику он меряет проход туда-обратно, чего обычный
 * push_rtp_packets не делает.
 *
 * Без этого GoogCC строит оценку по потерям и RTT, то есть узнаёт о заторе по его последствиям.
 */

/*
 * Проход туда-обратно, измеренный ОТПРАВЛЯЮЩЕЙ половиной по отчётам о доставке, — для диагностики
 * (`pathRtt`). Приёмной половине RTT больше не переносят (до ABI 17 это делала оболочка через
 * apply_measured_rtt_us): её модуль меряет его сам по XR RRTR/DLRR, как апстрим для потока без
 * отправки (`receiver_rtt_ms` в диагностике).
 * transport_rtt_us отдаёт -1, пока не измерено ни разу (ноль означал бы «меньше микросекунды»).
 */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_session_transport_rtt_us(
    OmniWebRtcMediaSession* session,
    int64_t* out_rtt_us);

OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_session_apply_transport_feedback(
    OmniWebRtcMediaSession* session,
    const uint8_t* data,
    size_t data_len);

/*
 * #5 increment 3a (2026-06-01): GPU video renderer (SwapChainPanel) session FFI. Additive
 * (no ABI bump). attach creates a composition swapchain sized width x height and returns its
 * IDXGISwapChain1* via out_swapchain (the C# layer binds it to a WinUI SwapChainPanel through
 * ISwapChainPanelNative::SetSwapChain on the UI thread, then Releases the returned pointer).
 * render_attached_video renders the most recent decoded frame to the swapchain + Presents;
 * resize matches the panel size; detach releases the swapchain. These calls must be SERIALISED
 * against each other by the caller, but not pinned to one thread: the D3D11 device is
 * multithread-protected (the decoder MFT already decodes on its own thread) and only
 * ISwapChainPanelNative::SetSwapChain — on the caller's side of the attach — needs the UI thread.
 * render_attached_video in particular should NOT run on a UI thread; present it on the display's
 * clock from a thread of its own (docs/decisions/video-presentation-thread.md).
 * NOT_IMPLEMENTED on non-Windows / no-libwebrtc builds.
 */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_session_attach_video_renderer(
    OmniWebRtcMediaSession* session,
    int32_t width,
    int32_t height,
    void** out_swapchain);

/*
 * `out_presented` (may be NULL) reports whether a NEW decoded frame actually reached the
 * swapchain this tick. "Nothing to draw" and "drew a frame" both return OK, so a caller that
 * needs to know whether remote video is still LIVE must read this rather than infer it from
 * the tick happening: the tick is a display-paced clock, so it keeps firing long after video
 * stopped.
 */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_session_render_attached_video(
    OmniWebRtcMediaSession* session,
    bool* out_presented);

OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_session_resize_attached_video(
    OmniWebRtcMediaSession* session,
    int32_t width,
    int32_t height);

OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_session_detach_video_renderer(
    OmniWebRtcMediaSession* session);

OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_session_update_abr(
    OmniWebRtcMediaSession* session,
    const OmniWebRtcMediaAbrSample* sample,
    OmniWebRtcMediaAbrTarget* target);

OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_session_drain_i420_frame(
    OmniWebRtcMediaSession* session,
    uint8_t* buffer,
    size_t buffer_len,
    int32_t* width,
    int32_t* height,
    uint32_t* rtp_timestamp,
    int64_t* timestamp_us,
    size_t* required_len);

OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_session_drain_bgra_frame(
    OmniWebRtcMediaSession* session,
    uint8_t* buffer,
    size_t buffer_len,
    int32_t* width,
    int32_t* height,
    uint32_t* rtp_timestamp,
    int64_t* timestamp_us,
    size_t* required_len);

OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_session_diagnostics_json(
    OmniWebRtcMediaSession* session,
    char* buffer,
    size_t buffer_len,
    size_t* required_len);

/*
 * Wave 3 (2026-05-16): libyuv-backed pixel ops. These replace the managed
 * bilinear in windows-app/Features/Calls/CallVideoFrameScaler.cs — moves
 * ~225 MB/s of pointer-arithmetic off the C# hot path onto native SSE2/AVX2
 * paths inside libyuv.
 *
 * All three exports return INVALID_ARGUMENT for null pointers / non-positive
 * dimensions, INTERNAL_ERROR on libyuv failure, NOT_IMPLEMENTED in builds
 * compiled without libwebrtc/libyuv linkage.
 *
 * Bilinear filter is used throughout (libyuv::kFilterBilinear); same quality
 * as the previous managed implementation.
 *
 * BGRA layout: byte order B, G, R, A (matches WriteableBitmap.PixelFormat
 * == Bgra8 and libyuv's "ARGB" naming on little-endian).
 */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_scale_bgra(
    const uint8_t* src_bgra,
    int32_t src_stride,
    int32_t src_width,
    int32_t src_height,
    uint8_t* dst_bgra,
    int32_t dst_stride,
    int32_t dst_width,
    int32_t dst_height);

OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_scale_nv12(
    const uint8_t* src_y,
    int32_t src_y_stride,
    const uint8_t* src_uv,
    int32_t src_uv_stride,
    int32_t src_width,
    int32_t src_height,
    uint8_t* dst_y,
    int32_t dst_y_stride,
    uint8_t* dst_uv,
    int32_t dst_uv_stride,
    int32_t dst_width,
    int32_t dst_height);

OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_nv12_to_bgra(
    const uint8_t* src_y,
    int32_t src_y_stride,
    const uint8_t* src_uv,
    int32_t src_uv_stride,
    uint8_t* dst_bgra,
    int32_t dst_stride,
    int32_t width,
    int32_t height);

/*
 * Audio Phase 2a-step3 (2026-05-21): in-process smoke shim exposed for
 * the CMake smoke target. Returns OK iff a freshly-created OmniAudioSender
 * encodes 20 ms of silence into at least one RTP packet whose header
 * carries version=2, payload type 111 (opus), and SSRC 0x4F4D4E41 ("OMNA").
 * Returns NOT_IMPLEMENTED on non-Windows / no-libwebrtc builds where the
 * sender + opus dependencies are not linked. Not part of the production
 * C# surface — the real send path lands in Phase 2a-step4.
 * See docs/decisions/audio-phase-1-adoption.md.
 */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_smoke_audio_roundtrip(void);

/*
 * Audio Phase 2a-step3 (2026-05-21): WASAPI capture → OmniAudioSender
 * wiring smoke. Constructs both, binds the capture callback to PushPcm,
 * Start/Sleep/Stop, then drains the sender. Returns OK on no crash and
 * — if any RTP packets were emitted — their headers carry the expected
 * audio version/PT/SSRC. Headless-safe: zero packets from a missing or
 * format-diverging capture endpoint is an accepted outcome. Returns
 * NOT_IMPLEMENTED on non-Windows / no-libwebrtc builds.
 */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_smoke_audio_capture_to_sender(void);

/*
 * Audio Phase 2b-step1 (2026-05-23): WASAPI render lifecycle smoke.
 * Start the render endpoint, write 20 ms of silent PCM, sleep, Stop.
 * Headless-safe: if no render endpoint exists Start fails and we
 * report the underlying HRESULT via stderr without flagging the smoke.
 * Returns NOT_IMPLEMENTED on non-Windows builds.
 */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_smoke_audio_render_lifecycle(void);

/*
 * Audio Phase 2b-step2 (2026-05-23): end-to-end opus + RTP roundtrip
 * via OmniAudioSender → OmniAudioReceiver. Returns OK iff the
 * receiver's PCM callback fires with the expected sample count
 * (kFrameSamples = 960). Validates the audio wire shape end-to-end
 * including PT, SSRC, and the opus codec roundtrip.
 */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_smoke_audio_sender_to_receiver(void);

/*
 * Audio under jitter, headless (2026-09-27): 10 s of a sine through the production Opus sender, then
 * packets delivered with 20% adjacent reordering and one 300 ms stall, and pulled in 10 ms steps on a
 * virtual clock — no device. Writes a JSON line of what the playout did into `report` (latency at the
 * end and at worst, audio produced beyond what was sent, silence played). OK when the receiver keeps
 * the stream bounded; the thresholds live in the implementation.
 */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_smoke_audio_jitter(
    char* report,
    size_t report_len);

/*
 * #5 increment 1a (2026-06-01): D3D11 offscreen render + readback smoke. Proves the GPU
 * render-target + staging readback path headlessly (no display). Returns OK on success,
 * INTERNAL_ERROR if the D3D device/readback fails, NOT_IMPLEMENTED off-Windows.
 * See docs/notes/video-gpu-render-swapchainpanel.md.
 */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_smoke_d3d11_solid_readback(void);

/*
 * #5 increment 1b (2026-06-01): GPU I420->RGB matches libyuv (golden, headless). Renders
 * uniform-color I420 frames through the BT.601 pixel shader and asserts each pixel is within
 * tolerance of libyuv::I420ToARGB. Validates the conversion math without a display. Needs
 * libyuv (libwebrtc build); returns NOT_IMPLEMENTED otherwise.
 */
/*
 * detail (optional, may be NULL) — то же, что у NV12-варианта, и по той же причине. Здесь тоже три
 * разных отказа — сбой рисования, сбой libyuv и настоящее расхождение пикселей — возвращались одним
 * INTERNAL_ERROR без единого слова. Флейк 2026-08-23 сработал именно в I420 и не сказал ничего,
 * тогда как NV12-вариант к тому моменту уже умел объяснять себя.
 */
/* ВИРТУАЛЬНЫЕ ЧАСЫ — ТОЛЬКО ДЛЯ СТЕНДОВ (#59).
 *
 * Пока они включены, всё внутри библиотеки живёт на времени, которое двигает вызывающий:
 * задачи очередей, таймеры пейсера и RTCP срабатывают внутри advance, а не сами по себе.
 * Прогон становится детерминированным и перестаёт зависеть от загрузки машины — ровно та
 * проблема, из-за которой замеры 2026-08-24 оказались несравнимыми между собой.
 *
 * Включать ДО создания сессий и выключать ПОСЛЕ их разрушения: подмена часов действует на
 * окружение, которое сессия берёт при создании.
 *
 * В продукте не используется. Аппаратный кодировщик и D3D11 живут на настоящем времени и под
 * виртуальными часами не меряются — эти стенды остаются на реальных. */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_test_simulated_time_begin(void);
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_test_simulated_time_advance_ms(
    int64_t duration_ms);
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_test_simulated_time_end(void);

OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_smoke_d3d11_i420_matches_libyuv(
    char* detail,
    size_t detail_len);

/*
 * #5 increment 2 (2026-06-01): composition-swapchain render path. Creates a
 * CreateSwapChainForComposition swapchain, renders an I420 frame to its backbuffer and (without
 * Present) reads it back, asserting it matches libyuv. Proves the swapchain render path
 * headlessly; SetSwapChain-to-panel + the visible Present are validated live. Swapchain-create
 * failure is tolerated (headless/locked GPUs). NOT_IMPLEMENTED without libwebrtc/libyuv.
 */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_smoke_d3d11_swapchain_i420(void);

/*
 * Task #84 (2026-06-06): HW H.264 decode (Media Foundation / DXVA, D3D11 mode) golden smoke.
 * MF-HW-encodes NV12 color-bars, HW-decodes them into an NV12 D3D11 texture, reads back the Y
 * plane and asserts the color-bar pattern survived. NOT_IMPLEMENTED (skip) when the machine has
 * no HW H.264 encoder/decoder MFT — HW decode is optional; SW OpenH264 is the mandatory fallback.
 * See docs/notes/hw-decode-CONTINUE.md.
 */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_smoke_mf_h264_hw_decode(void);

/*
 * Task #84 increment 2 (2026-06-06): GPU NV12->RGB matches libyuv (golden, headless). Validates
 * the BT.601 NV12 pixel shader + Y/UV plane SRVs against libyuv::NV12ToARGB — the HW decoder
 * outputs NV12 textures, this proves the renderer presents them correctly. NOT_IMPLEMENTED
 * without libwebrtc/libyuv.
 *
 * detail (optional, may be NULL) receives a short reason on failure: which stage broke and
 * on which colour case. Without it device-init failure, texture creation failure and an
 * actual pixel mismatch all came back as one INTERNAL_ERROR, and a flake could not be told
 * apart from a real conversion bug (2026-08-18: it flaked, and that is exactly what happened).
 */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_smoke_d3d11_nv12_matches_libyuv(
    char* detail,
    size_t detail_len);

/*
 * Audio Phase 2a-step4 (2026-05-23): opaque handle that owns a WASAPI
 * capture + opus sender pair. The C# layer creates one of these per
 * call; each encoded RTP packet (PT 111) goes to the packet sink set
 * with omni_webrtc_media_audio_capture_set_packet_sink. Lifecycle is independent from
 * OmniWebRtcMediaSession — audio doesn't need libwebrtc decoder /
 * pacing / NACK state, just the encoder + RTP packetizer + capture
 * worker. See docs/decisions/audio-phase-1-adoption.md.
 */
typedef struct OmniWebRtcMediaAudioCapture OmniWebRtcMediaAudioCapture;

/*
 * Audio endpoints a person can pick for calls (ABI 20, 2026-10-12), NUL-terminated JSON:
 *   {"capture":[{"id":"...","name":"...","default":true},...],"render":[...]}
 * Only active endpoints. `id` is the IMMDevice id that capture_create / playback_create take as
 * "device_id"; `default` marks Windows' default device (eConsole) — the one opened without a choice.
 * Returns BUFFER_TOO_SMALL with the needed size in `out_written` when the buffer is short.
 */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_audio_list_devices(
    char* buffer,
    size_t buffer_len,
    size_t* out_written);

/*
 * Creates an audio capture+send pipeline. Config JSON keys:
 *   "target_bitrate_bps" (int, default 32000, clamped to [6000, 510000]).
 *   "device_id" (string, optional) — microphone from omni_webrtc_media_audio_list_devices. A gone
 *     or inactive device falls back to the default one: a stale choice never leaves a call mute-less.
 *
 * On success, *out_handle is set to a non-null pointer that the caller
 * must dispose via omni_webrtc_media_audio_capture_destroy.
 *
 * On failure, writes a diagnostic to error_buffer (with the standard
 * required_error_len sizing protocol) and returns INTERNAL_ERROR. The
 * most common failure mode is WASAPI Start returning a COM HRESULT
 * (no audio session subsystem, denied access, etc.).
 *
 * Returns NOT_IMPLEMENTED on non-Windows builds.
 */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_audio_capture_create(
    const char* config_json,
    OmniWebRtcMediaAudioCapture** out_handle,
    char* error_buffer,
    size_t error_buffer_len,
    size_t* required_error_len);

/*
 * Stops capture, joins the worker, releases the opus encoder. Idempotent
 * on null. After this call the handle is invalid.
 */
OMNI_WEBRTC_MEDIA_API void omni_webrtc_media_audio_capture_destroy(
    OmniWebRtcMediaAudioCapture* handle);

/* Where the encoded audio RTP packets go (see OmniWebRtcMediaPacketSink). */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_audio_capture_set_packet_sink(
    OmniWebRtcMediaAudioCapture* handle,
    OmniWebRtcMediaPacketSink sink,
    void* context);

/*
 * Muted: capture, echo cancellation and the speaking level keep running, nothing is sent.
 * Non-zero = muted.
 */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_audio_capture_set_muted(
    OmniWebRtcMediaAudioCapture* handle,
    int32_t muted);

/* Audio RTP packets handed to the packet sink so far (muted packets are not counted). */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_audio_capture_packets_sent(
    OmniWebRtcMediaAudioCapture* handle,
    uint64_t* out_sent);

/*
 * Group-call speaking indicator: smoothed RMS (0..1) of the mic audio
 * currently being sent (0 when muted / no signal). The self tile polls
 * this to light its speaking ring. Cheap atomic read.
 */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_audio_capture_level(
    OmniWebRtcMediaAudioCapture* handle,
    float* out_level);

/* How long the microphone's raw input (before noise suppression) has been pure digital zero, in
 * ms: since the last non-zero sample, or since capture started (ABI 21). */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_audio_capture_digital_silence_ms(
    OmniWebRtcMediaAudioCapture* handle,
    int64_t* out_ms);

/*
 * One line of echo-canceller state for the call diagnostics, NUL-terminated:
 *   `aec=off(apm_unavailable)`                — no cancellation at all
 *   `aec=on reverse_blocks=N erl=.. erle=.. delay=..ms residual=..`
 *
 * `reverse_blocks` is the number of 10 ms far-end blocks handed to the canceller since this
 * capture session started; zero means nothing is feeding it the speaker signal, so it cannot
 * cancel anything. `erle` is how much echo it is actually removing, in dB.
 *
 * Exists because an echo complaint is otherwise undiagnosable: a canceller that is never fed
 * looks exactly like one that is fed and failing. Returns BUFFER_TOO_SMALL with the needed
 * size in `out_written` when the buffer is short.
 */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_audio_capture_echo_diagnostics(
    OmniWebRtcMediaAudioCapture* handle,
    char* buffer,
    size_t buffer_len,
    size_t* out_written);

/*
 * Audio Phase 2b-step3 (2026-05-23): opaque handle that owns an opus
 * decoder + WASAPI render endpoint. The C# layer creates one per call
 * (alongside the AudioCapture for the send side) and pushes inbound
 * RTP packets via push_rtp_packet. The handle internally drops
 * mismatched-PT/SSRC packets silently so the caller can fire-and-
 * forget every inbound datagram, regardless of whether it's audio or
 * video — the video receiver gets the same packets and decodes them
 * separately.
 */
typedef struct OmniWebRtcMediaAudioPlayback OmniWebRtcMediaAudioPlayback;

/*
 * Config JSON keys:
 *   "render" (int, default 1) — 0 = decode-only level probe, no device opened.
 *   "device_id" (string, optional) — speakers from omni_webrtc_media_audio_list_devices; a gone or
 *     inactive device falls back to the default one.
 */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_audio_playback_create(
    const char* config_json,
    OmniWebRtcMediaAudioPlayback** out_handle,
    char* error_buffer,
    size_t error_buffer_len,
    size_t* required_error_len);

OMNI_WEBRTC_MEDIA_API void omni_webrtc_media_audio_playback_destroy(
    OmniWebRtcMediaAudioPlayback* handle);

OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_audio_playback_push_rtp_packet(
    OmniWebRtcMediaAudioPlayback* handle,
    const uint8_t* packet,
    size_t packet_len);

/*
 * Packets the playback took into its jitter buffer (NetEq) vs. ones that were not its audio (the
 * router hands every inbound packet of a 1:1 call to both video and audio; video and RTCP land in
 * `packets_dropped` here by design).
 */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_audio_playback_read_counters(
    OmniWebRtcMediaAudioPlayback* handle,
    uint64_t* packets_accepted,
    uint64_t* packets_dropped);

/*
 * Group-call active-speaker: smoothed RMS (0..1) of this sender's
 * decoded audio. The per-sender remote tile polls this to light its
 * speaking ring and to auto-focus the loudest participant. Cheap
 * atomic read.
 */
/*
 * The jitter buffer (NetEq, ABI 15, 2026-09-27): what it holds now and aims at, and what it has
 * done to keep that — concealed (no audio to play), accelerated away (catching up after a burst),
 * decelerated in (stretching to build headroom). Cumulative ms since the playback was created.
 */
typedef struct OmniWebRtcMediaAudioJitterStats {
    uint32_t buffer_ms;
    uint32_t target_ms;
    uint64_t concealed_ms;
    uint64_t accelerated_ms;
    uint64_t decelerated_ms;
} OmniWebRtcMediaAudioJitterStats;

OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_audio_playback_jitter_stats(
    OmniWebRtcMediaAudioPlayback* handle,
    OmniWebRtcMediaAudioJitterStats* out_stats);

OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_audio_playback_level(
    OmniWebRtcMediaAudioPlayback* handle,
    float* out_level);

/*
 * Lipsync: the video session's playout target becomes the jitter buffer's MINIMUM delay (NetEq
 * SetMinimumDelay, as upstream AudioReceiveStream does), so audio does not run ahead of video;
 * NetEq still adds headroom above it when the path needs it. Clamped at 500 ms; *applied_delay_ms
 * returns the value applied.
 */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_audio_playback_set_target_playout_delay_ms(
    OmniWebRtcMediaAudioPlayback* handle,
    uint32_t delay_ms,
    uint32_t* applied_delay_ms);

/*
 * INBOUND MEDIA ROUTER (ABI 14, 2026-09-27). The core hands every inbound datagram of a call to
 * omni_webrtc_media_router_deliver (same shape as OmniWebRtcMediaPacketSink) with the router as
 * context, at once, on its own thread — wired by omni_realtime_media_set_inbound_sink. The router
 * routes as the shell used to: 1:1 — the delivery report (RTCP 205/11) to the sender session, the
 * rest to the video receiver and the audio playback; group — the 8-byte sender tag picks that
 * sender's receiver and level probe, audio also goes to the shared playback.
 *
 * Sessions are built by the shell; the router only records that one is wanted (take_requests).
 * Every set_* returns only when no delivery is in flight, so the detached object may be destroyed
 * right after. Detach from the core (unwire the inbound sink) before destroying the router.
 */
#define OMNI_WEBRTC_MEDIA_ROUTER_MAX_WANTED_GROUP_SENDERS 16u

typedef struct OmniWebRtcMediaRouter OmniWebRtcMediaRouter;

typedef struct OmniWebRtcMediaRouterRequests {
    int32_t video_receiver_wanted;
    uint32_t group_sender_count;
    uint8_t group_sender_tags[OMNI_WEBRTC_MEDIA_ROUTER_MAX_WANTED_GROUP_SENDERS][8];
    /* ABI 22: each wanted sender's latest video payload type (0 = no video yet). A sender already
     * wired is asked for again when its video arrives in a codec its receiver was not built for. */
    uint8_t group_sender_video_pt[OMNI_WEBRTC_MEDIA_ROUTER_MAX_WANTED_GROUP_SENDERS];
} OmniWebRtcMediaRouterRequests;

OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_router_create(
    OmniWebRtcMediaRouter** out_router);
OMNI_WEBRTC_MEDIA_API void omni_webrtc_media_router_destroy(OmniWebRtcMediaRouter* router);
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_router_set_sender(
    OmniWebRtcMediaRouter* router,
    OmniWebRtcMediaSession* sender);
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_router_set_receiver(
    OmniWebRtcMediaRouter* router,
    OmniWebRtcMediaSession* receiver);
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_router_set_audio_playback(
    OmniWebRtcMediaRouter* router,
    OmniWebRtcMediaAudioPlayback* playback);
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_router_set_group_mode(
    OmniWebRtcMediaRouter* router,
    int32_t group_mode);
/* Both NULL removes the sender. */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_router_set_group_sender(
    OmniWebRtcMediaRouter* router,
    const uint8_t* sender_tag,
    OmniWebRtcMediaSession* receiver,
    OmniWebRtcMediaAudioPlayback* level_probe);
/* What arrived with nobody to take it since the last call; clears the record. */
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_router_take_requests(
    OmniWebRtcMediaRouter* router,
    OmniWebRtcMediaRouterRequests* out_requests);
OMNI_WEBRTC_MEDIA_API OmniWebRtcMediaStatus omni_webrtc_media_router_counters(
    OmniWebRtcMediaRouter* router,
    uint64_t* out_delivered,
    uint64_t* out_feedback,
    uint64_t* out_dropped);
OMNI_WEBRTC_MEDIA_API void omni_webrtc_media_router_deliver(
    void* context,
    const uint8_t* packet,
    size_t packet_len);

#ifdef __cplusplus
}
#endif
