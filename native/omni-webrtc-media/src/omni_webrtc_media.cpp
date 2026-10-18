#include "omni_webrtc_media.h"

#include <algorithm>
#include <array>
#include <cctype>
#include <chrono>
#include <cmath>
#include <condition_variable>
#include <cstdint>
#include <cstring>
#include <new>
#include <deque>
#include <future>
#include <functional>
#include <iomanip>
#include <memory>
#include <map>
#include <atomic>
#include <mutex>
#include <optional>
#include <set>
#include <span>
#include <sstream>
#include <string>
#include <string_view>
#include <thread>
#include <utility>
#include <variant>
#include <vector>

#if defined(_WIN32)
#include <codecapi.h>
#include <mfapi.h>
#include <mfidl.h>
#include <mferror.h>
#include <mfobjects.h>
#include <mftransform.h>
#include <strmif.h>
#include <windows.h>
#include <wrl/client.h>
// Audio Phase 2a-step2 (2026-05-21): WASAPI shared-mode capture.
// audioclient.h pulls IAudioClient / IAudioCaptureClient,
// mmdeviceapi.h pulls IMMDeviceEnumerator / IMMDevice. avrt.h
// would give MMCSS priority hooks if we later need them.
#include <audioclient.h>
#include <mmdeviceapi.h>
#include <functiondiscoverykeys_devpkey.h>
#include "d3d11_video_renderer.h"
#endif

#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
#include "api/environment/environment_factory.h"
#include "api/field_trials.h"
#include "test/time_controller/simulated_time_controller.h"
#include "api/audio/audio_processing.h"
#include "api/audio/builtin_audio_processing_builder.h"
#include "api/video/encoded_image.h"
#include "api/video/i420_buffer.h"
#include "api/video/nv12_buffer.h"
#include "api/video_codecs/video_encoder_software_fallback_wrapper.h"
#include "video/video_stream_encoder.h"
#include "video/video_stream_encoder_observer.h"
#include "video/frame_cadence_adapter.h"
#include "api/video/video_bitrate_allocator_factory.h"
#include "api/video/builtin_video_bitrate_allocator_factory.h"
#include "api/video/rtp_video_frame_assembler.h"
#include "api/video/video_bitrate_allocation.h"
#include "api/video/video_codec_type.h"
#include "api/video/video_frame.h"
#include "api/video/video_frame_type.h"
#include "api/video_codecs/builtin_video_decoder_factory.h"
#include "api/video_codecs/builtin_video_encoder_factory.h"
#include "api/video_codecs/sdp_video_format.h"
#include "api/video_codecs/video_codec.h"
#include "api/video_codecs/video_decoder_factory.h"
#include "api/video_codecs/video_encoder.h"
#include "api/video_codecs/video_encoder_factory.h"
#include "common_video/h264/h264_common.h"
#include "libyuv/convert_from.h"
#include "libyuv/convert_from_argb.h"
#include "libyuv/convert.h"
#include "libyuv/convert_argb.h"
#include "libyuv/scale.h"
#include "libyuv/scale_argb.h"
// Wave 4 Phase 2a (2026-05-16): GoogCC infrastructure. Controller is
// constructed in Initialize() but UpdateAbr still uses the legacy EWMA
// state machine — switch-over happens in Phase 2b after build linkage
// is verified. See docs/decisions/googcc-adoption.md.
#include "api/transport/goog_cc_factory.h"
#include "api/transport/network_control.h"
#include "api/transport/network_types.h"
// Wave 5 Phase 5a (2026-05-16): PacingController scaffolding. Controller is
// constructed in Initialize() but UpdateAbr/encoder output don't yet route
// through it — Phase 5a-active commit will wire EnqueuePacket + ProcessPackets.
// See docs/decisions/pacing-nack-playout-adoption.md.
#include "modules/pacing/pacing_controller.h"
#include "modules/rtp_rtcp/source/rtp_format.h"
#include "modules/rtp_rtcp/source/frame_object.h"
#include "modules/rtp_rtcp/source/rtp_packet_received.h"
#include "api/audio/audio_frame.h"
#include "api/audio_codecs/opus_audio_decoder_factory.h"
#include "api/neteq/default_neteq_factory.h"
#include "api/neteq/neteq.h"
#include "system_wrappers/include/clock.h"
#include "modules/rtp_rtcp/source/rtp_packet_to_send.h"
// #50 этап 1: апстримовый RTP-отправитель. Объектные файлы уже собраны GN, glob в
// CMakeLists добавлен отдельным шагом — линковка 98 объектов прошла без конфликтов.
#include "api/call/transport.h"
#include "modules/rtp_rtcp/source/rtp_rtcp_impl2.h"
#include "modules/rtp_rtcp/include/receive_statistics.h"
#include "modules/pacing/packet_router.h"
#include "modules/pacing/task_queue_paced_sender.h"
#include "modules/rtp_rtcp/source/rtp_video_header.h"
#include "modules/rtp_rtcp/source/rtp_sender_video.h"
#include "call/rtx_receive_stream.h"
#include "call/rtp_transport_controller_send.h"
#include "call/rtp_transport_config.h"
#include "call/rtp_config.h"
#include "call/rtp_video_sender_interface.h"
#include "modules/video_coding/fec_controller_default.h"
#include "api/transport/bitrate_settings.h"
#include "call/rtp_packet_sink_interface.h"
#include "modules/video_coding/h264_sps_pps_tracker.h"
#include "modules/video_coding/include/video_error_codes.h"
// ДЕТЕКТОРЫ АДАПТАЦИИ ЖИВУТ ВНУТРИ ОРКЕСТРАТОРА КОДИРОВАНИЯ (#61, шаг D3).
//
// Здесь брались OveruseFrameDetector и QualityScaler по отдельности, а решения по их сигналам
// принимал наш такт ABR. Теперь обоими владеет VideoStreamEncoder через
// VideoStreamEncoderResourceManager, и он же переводит их сигналы в пожелания источнику —
// сколько точек и сколько кадров в секунду он готов принять. Детектор перегрузки остаётся
// здесь только как заголовок: экземпляр создаём мы и отдаём оркестратору во владение.
#include "api/task_queue/task_queue_base.h"
#include "api/task_queue/task_queue_factory.h"
#include "video/adaptation/overuse_frame_detector.h"
#include "modules/congestion_controller/rtp/transport_feedback_adapter.h"
#include "modules/remote_bitrate_estimator/congestion_control_feedback_generator.h"
#include "modules/rtp_rtcp/source/rtcp_packet/congestion_control_feedback.h"
#include "modules/rtp_rtcp/source/rtcp_packet/common_header.h"
#include "modules/rtp_rtcp/source/rtp_generic_frame_descriptor.h"
#include "modules/rtp_rtcp/source/rtp_generic_frame_descriptor_extension.h"
#include "modules/rtp_rtcp/source/rtp_header_extensions.h"
#include "rtc_base/experiments/encoder_info_settings.h"
#include "video/encoder_bitrate_adjuster.h"
#include "video/video_stream_encoder_observer.h"
// Wave 5b (2026-05-18): NackRequester (libwebrtc's NACK module) replaces
// the self-written gap-tracker / retry-loop / pruning code. Its NACKs and
// keyframe requests leave as real RTCP from the receive module (step A2,
// 2026-10-02; see OmniReceiverRtcpFeedback). The shared NackPeriodicProcessor
// ticks ProcessNacks every 20 ms on our adaptation task queue.
// See docs/decisions/pacing-nack-playout-adoption.md.
#include "modules/rtp_rtcp/include/rtcp_statistics.h"
#include "modules/rtp_rtcp/include/report_block_data.h"
#include "video/encoder_rtcp_feedback.h"
#include "modules/include/module_common_types.h"
#include "modules/video_coding/nack_requester.h"
#include "modules/video_coding/timing/timing.h"
#include "video/task_queue_frame_decode_scheduler.h"
#include "api/rtc_event_log/rtc_event_log.h"
#include "rtc_base/numerics/sequence_number_unwrapper.h"
#include "logging/rtc_event_log/events/rtc_event_bwe_update_delay_based.h"
#include "logging/rtc_event_log/events/rtc_event_bwe_update_loss_based.h"
#include "logging/rtc_event_log/events/rtc_event_probe_result_failure.h"
#include "logging/rtc_event_log/events/rtc_event_probe_result_success.h"
#include "video/video_receive_stream_timeout_tracker.h"
#include "video/video_stream_buffer_controller.h"
// Audio Phase 2a-step3 (2026-05-21): libopus encoder for the audio
// pipeline. Headers shipped with the libwebrtc third_party/opus tree
// we already link in step1. See docs/decisions/audio-phase-1-adoption.md.
#include "opus.h"
// Wave 5c (2026-05-18): FrameBuffer (api/video/) is the modern libwebrtc
// jitter buffer — stores up to N inflight frames, tracks decodability via
// frame ID references, returns temporal units ready to decode. Pull-based
// (no callbacks); we own the synchronization through mutex_. JitterEstimator
// (modules/video_coding/timing/) consumes per-frame {arrival_delay, size}
// samples and emits an adaptive target playout delay that replaces the
// old hardcoded kPlayoutDelayMs = 60 ms.
// See docs/decisions/pacing-nack-playout-adoption.md (Phase 5c).
#include "absl/container/inlined_vector.h"
#include "api/video/frame_buffer.h"
#include "api/units/data_size.h"
#include "modules/video_coding/timing/jitter_estimator.h"
#endif

struct OmniWebRtcMediaSession {
    void* impl = nullptr;
};

// Audio Phase 2a-step4 (2026-05-23): opaque handle for the audio
// capture+send pipeline. Owns a WASAPI capture and an opus sender
// connected by a closure on the capture's worker thread. Kept
// independent from OmniWebRtcMediaSession — audio doesn't need any
// libwebrtc decoder / pacing / NACK / GoogCC state.
struct OmniWebRtcMediaAudioCapture {
    void* impl = nullptr;
};

// Audio Phase 2b-step3 (2026-05-23): opaque handle for the audio
// receive+playback pipeline. Owns the NetEq receiver and (unless decode-only) a WASAPI render
// endpoint that pulls from it.
struct OmniWebRtcMediaAudioPlayback {
    void* impl = nullptr;
};

namespace {

#if !defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
constexpr std::string_view kProbeJson =
    R"({"abi_version":23,"available":false,"backend":"none","transport":"omni_quic_datagram_control","supports_peer_connection":false,"supports_rtp_over_omni_quic":false,"supports_rtcp_feedback":false,"hardware_codecs":[],"software_codecs":[],"reason":"libwebrtc backend is not linked yet"})";

constexpr std::string_view kNoBackendSessionNotImplemented =
    "omni_webrtc_media native ABI is present, but no libwebrtc media backend is linked yet.";
#endif

#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
struct CodecSmokeResult {
    bool ok = false;
    size_t encoded_bytes = 0;
    std::string diagnostics;
};

struct EncodedFrame {
    std::vector<uint8_t> data;
    webrtc::VideoFrameType frame_type = webrtc::VideoFrameType::kVideoFrameDelta;
    uint32_t rtp_timestamp = 0;
    // Wave 6 (2026-05-18): QP carried out of OnEncodedImage so the encode
    // path can hand it to QualityScaler::ReportQp(). nullopt means the
    // encoder didn't report a QP for this frame (some hardware encoders
    // don't expose it without a bitstream parser); the scaler simply skips.
    std::optional<uint8_t> qp;
    // Wave 6: when the frame was first observed by the capture pipeline,
    // in microseconds (monotonic). Used for OveruseFrameDetector::FrameSent's
    // capture_time_us parameter so the CPU detector can compute encode
    // latency against the capture clock, not against when the encoder
    // happened to dequeue the frame.
    int64_t capture_time_us = 0;
};

/// QP OF AN AV1 FRAME, READ FROM ITS OWN BITSTREAM.
///
/// The hardware encoders (Media Foundation MFTs) do not report QP — NVIDIA leaves
/// MFSampleExtension_VideoEncodeQP unset — and without it libwebrtc falls back to judging the frame
/// size by the encoder's output bitrate against an H.264 table. AV1 spends far fewer bits on a calm
/// camera scene than H.264, so that table read "720p is starving" and shrank the picture while the
/// target had room (docs/bugs/hardware-encoder-shrank-calm-camera.md). For H.264 and H.265
/// libwebrtc parses QP itself (QpParser); for AV1 it does not, so this does.
///
/// The value is base_q_idx (0..255) — the scale of libaom's AOME_GET_LAST_QUANTIZER, which is what
/// the AV1 QualityScaler thresholds (145/205) expect. Follows the AV1 spec, sections 5.5 (sequence
/// header) and 5.9 (uncompressed frame header) up to quantization_params(). A frame whose size
/// comes from a reference (frame_size_with_refs with found_ref) needs decoder state this parser
/// does not keep: it gives no QP for that frame rather than a guess.
class Av1QpParser {
public:
    /// The QP of the first frame header in `data` (low-overhead OBUs). Remembers a sequence header
    /// it sees, since a delta frame's header can only be read with one.
    std::optional<int> Parse(const uint8_t* data, size_t size)
    {
        size_t offset = 0;
        while (offset < size) {
            const uint8_t header = data[offset];
            const uint8_t type = (header >> 3) & 0x0Fu;
            const bool has_extension = (header & 0x04u) != 0;
            const bool has_size = (header & 0x02u) != 0;
            size_t cursor = offset + 1;
            int temporal_id = 0;
            int spatial_id = 0;
            if (has_extension) {
                if (cursor >= size) {
                    return std::nullopt;
                }
                temporal_id = data[cursor] >> 5;
                spatial_id = (data[cursor] >> 3) & 0x3;
                ++cursor;
            }
            if (!has_size) {
                return std::nullopt;
            }
            uint64_t obu_size = 0;
            bool size_ok = false;
            for (int shift = 0; cursor < size && shift < 56; shift += 7) {
                const uint8_t byte = data[cursor++];
                obu_size |= static_cast<uint64_t>(byte & 0x7Fu) << shift;
                if ((byte & 0x80u) == 0) {
                    size_ok = true;
                    break;
                }
            }
            if (!size_ok || obu_size > size - cursor) {
                return std::nullopt;
            }
            BitReader reader(data + cursor, static_cast<size_t>(obu_size));
            if (type == kObuSequenceHeader) {
                Sequence sequence;
                if (ReadSequenceHeader(reader, sequence)) {
                    sequence_ = sequence;
                }
            } else if (type == kObuFrameHeader || type == kObuFrame) {
                if (!sequence_.has_value()) {
                    return std::nullopt;
                }
                return ReadBaseQIdx(reader, *sequence_, temporal_id, spatial_id);
            }
            offset = cursor + static_cast<size_t>(obu_size);
        }
        return std::nullopt;
    }

private:
    static constexpr uint8_t kObuSequenceHeader = 1;
    static constexpr uint8_t kObuFrameHeader = 3;
    static constexpr uint8_t kObuFrame = 6;
    static constexpr int kSelect = 2;  // SELECT_SCREEN_CONTENT_TOOLS, SELECT_INTEGER_MV
    static constexpr int kKeyFrame = 0;
    static constexpr int kIntraOnlyFrame = 2;
    static constexpr int kSwitchFrame = 3;

    class BitReader {
    public:
        BitReader(const uint8_t* data, size_t size) : data_(data), size_(size) {}
        bool ok() const { return ok_; }
        uint32_t f(int bits)
        {
            uint32_t value = 0;
            for (int i = 0; i < bits; ++i) {
                if (position_ >= size_ * 8) {
                    ok_ = false;
                    return 0;
                }
                const uint32_t bit = (data_[position_ >> 3] >> (7 - (position_ & 7))) & 1u;
                value = (value << 1) | bit;
                ++position_;
            }
            return value;
        }
        uint32_t uvlc()
        {
            int leading_zeros = 0;
            while (ok_ && f(1) == 0) {
                if (++leading_zeros >= 32) {
                    ok_ = false;
                    return 0;
                }
            }
            return leading_zeros == 0 ? 0 : f(leading_zeros) + (1u << leading_zeros) - 1;
        }
        /// ns(n): a value in 0..n-1, coded with as few bits as the spec allows.
        uint32_t ns(uint32_t n)
        {
            int w = 0;
            for (uint32_t x = n; x != 0; x >>= 1) {
                ++w;
            }
            const uint32_t m = (1u << w) - n;
            const uint32_t v = f(w - 1);
            if (v < m) {
                return v;
            }
            return (v << 1) - m + f(1);
        }

    private:
        const uint8_t* data_;
        size_t size_;
        size_t position_ = 0;
        bool ok_ = true;
    };

    struct Sequence {
        bool reduced_still_picture_header = false;
        bool decoder_model_info_present = false;
        bool equal_picture_interval = false;
        int buffer_removal_time_length = 0;
        int frame_presentation_time_length = 0;
        int operating_points = 1;
        int operating_point_idc[32] = {};
        bool decoder_model_present_for_op[32] = {};
        int frame_width_bits = 0;
        int frame_height_bits = 0;
        int max_frame_width = 0;
        int max_frame_height = 0;
        bool frame_id_numbers_present = false;
        int delta_frame_id_length = 0;
        int frame_id_length = 0;
        bool use_128x128_superblock = false;
        bool enable_order_hint = false;
        bool enable_ref_frame_mvs = false;
        int seq_force_screen_content_tools = kSelect;
        int seq_force_integer_mv = kSelect;
        int order_hint_bits = 0;
        bool enable_superres = false;
    };

    static bool ReadSequenceHeader(BitReader& r, Sequence& s)
    {
        (void)r.f(3);  // seq_profile
        (void)r.f(1);  // still_picture
        s.reduced_still_picture_header = r.f(1) != 0;
        if (s.reduced_still_picture_header) {
            (void)r.f(5);  // seq_level_idx[0]
        } else {
            int buffer_delay_length = 0;
            if (r.f(1) != 0) {  // timing_info_present_flag
                (void)r.f(32);  // num_units_in_display_tick
                (void)r.f(32);  // time_scale
                s.equal_picture_interval = r.f(1) != 0;
                if (s.equal_picture_interval) {
                    (void)r.uvlc();  // num_ticks_per_picture_minus_1
                }
                s.decoder_model_info_present = r.f(1) != 0;
                if (s.decoder_model_info_present) {
                    buffer_delay_length = static_cast<int>(r.f(5)) + 1;
                    (void)r.f(32);  // num_units_in_decoding_tick
                    s.buffer_removal_time_length = static_cast<int>(r.f(5)) + 1;
                    s.frame_presentation_time_length = static_cast<int>(r.f(5)) + 1;
                }
            }
            const bool initial_display_delay_present = r.f(1) != 0;
            s.operating_points = static_cast<int>(r.f(5)) + 1;
            for (int i = 0; i < s.operating_points; ++i) {
                s.operating_point_idc[i] = static_cast<int>(r.f(12));
                if (r.f(5) > 7) {  // seq_level_idx
                    (void)r.f(1);  // seq_tier
                }
                if (s.decoder_model_info_present) {
                    s.decoder_model_present_for_op[i] = r.f(1) != 0;
                    if (s.decoder_model_present_for_op[i]) {
                        (void)r.f(buffer_delay_length);  // decoder_buffer_delay
                        (void)r.f(buffer_delay_length);  // encoder_buffer_delay
                        (void)r.f(1);                    // low_delay_mode_flag
                    }
                }
                if (initial_display_delay_present && r.f(1) != 0) {
                    (void)r.f(4);  // initial_display_delay_minus_1
                }
            }
        }
        s.frame_width_bits = static_cast<int>(r.f(4)) + 1;
        s.frame_height_bits = static_cast<int>(r.f(4)) + 1;
        s.max_frame_width = static_cast<int>(r.f(s.frame_width_bits)) + 1;
        s.max_frame_height = static_cast<int>(r.f(s.frame_height_bits)) + 1;
        s.frame_id_numbers_present = !s.reduced_still_picture_header && r.f(1) != 0;
        if (s.frame_id_numbers_present) {
            s.delta_frame_id_length = static_cast<int>(r.f(4)) + 2;
            s.frame_id_length = static_cast<int>(r.f(3)) + 1 + s.delta_frame_id_length;
        }
        s.use_128x128_superblock = r.f(1) != 0;
        (void)r.f(1);  // enable_filter_intra
        (void)r.f(1);  // enable_intra_edge_filter
        if (!s.reduced_still_picture_header) {
            (void)r.f(1);  // enable_interintra_compound
            (void)r.f(1);  // enable_masked_compound
            (void)r.f(1);  // enable_warped_motion
            (void)r.f(1);  // enable_dual_filter
            s.enable_order_hint = r.f(1) != 0;
            if (s.enable_order_hint) {
                (void)r.f(1);  // enable_jnt_comp
                s.enable_ref_frame_mvs = r.f(1) != 0;
            }
            s.seq_force_screen_content_tools = r.f(1) != 0 ? kSelect : static_cast<int>(r.f(1));
            if (s.seq_force_screen_content_tools > 0) {
                s.seq_force_integer_mv = r.f(1) != 0 ? kSelect : static_cast<int>(r.f(1));
            }
            if (s.enable_order_hint) {
                s.order_hint_bits = static_cast<int>(r.f(3)) + 1;
            }
        }
        s.enable_superres = r.f(1) != 0;
        return r.ok();
    }

    static int TileLog2(int block_size, int target)
    {
        int k = 0;
        while ((block_size << k) < target) {
            ++k;
        }
        return k;
    }

    // frame_size() + superres_params() + compute_image_size(): leaves the frame's size in 4x4 units.
    // Returns use_superres (UpscaledWidth != FrameWidth).
    static bool ReadFrameSize(BitReader& r, const Sequence& s, bool frame_size_override,
                              int& mi_cols, int& mi_rows)
    {
        bool use_superres = false;
        int width = s.max_frame_width;
        int height = s.max_frame_height;
        if (frame_size_override) {
            width = static_cast<int>(r.f(s.frame_width_bits)) + 1;
            height = static_cast<int>(r.f(s.frame_height_bits)) + 1;
        }
        if (s.enable_superres && r.f(1) != 0) {
            use_superres = true;
            const int denominator = static_cast<int>(r.f(3)) + 9;
            width = (width * 8 + denominator / 2) / denominator;
        }
        mi_cols = 2 * ((width + 7) >> 3);
        mi_rows = 2 * ((height + 7) >> 3);
        return use_superres;
    }

    static void ReadRenderSize(BitReader& r)
    {
        if (r.f(1) != 0) {  // render_and_frame_size_different
            (void)r.f(16);
            (void)r.f(16);
        }
    }

    static void ReadTileInfo(BitReader& r, const Sequence& s, int mi_cols, int mi_rows)
    {
        const int sb_cols = s.use_128x128_superblock ? ((mi_cols + 31) >> 5) : ((mi_cols + 15) >> 4);
        const int sb_rows = s.use_128x128_superblock ? ((mi_rows + 31) >> 5) : ((mi_rows + 15) >> 4);
        const int sb_size = (s.use_128x128_superblock ? 5 : 4) + 2;
        const int max_tile_width_sb = 4096 >> sb_size;
        int max_tile_area_sb = (4096 * 2304) >> (2 * sb_size);
        const int min_log2_tile_cols = TileLog2(max_tile_width_sb, sb_cols);
        const int max_log2_tile_cols = TileLog2(1, std::min(sb_cols, 64));
        const int max_log2_tile_rows = TileLog2(1, std::min(sb_rows, 64));
        const int min_log2_tiles = std::max(min_log2_tile_cols, TileLog2(max_tile_area_sb, sb_rows * sb_cols));
        int tile_cols_log2 = 0;
        int tile_rows_log2 = 0;
        if (r.f(1) != 0) {  // uniform_tile_spacing_flag
            tile_cols_log2 = min_log2_tile_cols;
            while (r.ok() && tile_cols_log2 < max_log2_tile_cols && r.f(1) != 0) {
                ++tile_cols_log2;
            }
            tile_rows_log2 = std::max(min_log2_tiles - tile_cols_log2, 0);
            while (r.ok() && tile_rows_log2 < max_log2_tile_rows && r.f(1) != 0) {
                ++tile_rows_log2;
            }
        } else {
            int widest_tile_sb = 0;
            int tile_cols = 0;
            for (int start = 0; r.ok() && start < sb_cols; ++tile_cols) {
                const int size_sb = static_cast<int>(r.ns(static_cast<uint32_t>(
                    std::min(sb_cols - start, max_tile_width_sb)))) + 1;
                widest_tile_sb = std::max(size_sb, widest_tile_sb);
                start += size_sb;
            }
            tile_cols_log2 = TileLog2(1, tile_cols);
            max_tile_area_sb = min_log2_tiles > 0
                ? (sb_rows * sb_cols) >> (min_log2_tiles + 1)
                : sb_rows * sb_cols;
            const int max_tile_height_sb = std::max(max_tile_area_sb / std::max(widest_tile_sb, 1), 1);
            int tile_rows = 0;
            for (int start = 0; r.ok() && start < sb_rows; ++tile_rows) {
                start += static_cast<int>(r.ns(static_cast<uint32_t>(
                    std::min(sb_rows - start, max_tile_height_sb)))) + 1;
            }
            tile_rows_log2 = TileLog2(1, tile_rows);
        }
        if (tile_cols_log2 > 0 || tile_rows_log2 > 0) {
            (void)r.f(tile_rows_log2 + tile_cols_log2);  // context_update_tile_id
            (void)r.f(2);                                // tile_size_bytes_minus_1
        }
    }

    static std::optional<int> ReadBaseQIdx(BitReader& r, const Sequence& s, int temporal_id, int spatial_id)
    {
        int frame_type = kKeyFrame;
        bool show_frame = true;
        bool error_resilient_mode = true;
        if (!s.reduced_still_picture_header) {
            if (r.f(1) != 0) {  // show_existing_frame: no new picture, no QP
                return std::nullopt;
            }
            frame_type = static_cast<int>(r.f(2));
            show_frame = r.f(1) != 0;
            if (show_frame && s.decoder_model_info_present && !s.equal_picture_interval) {
                (void)r.f(s.frame_presentation_time_length);
            }
            if (!show_frame) {
                (void)r.f(1);  // showable_frame
            }
            error_resilient_mode = (frame_type == kSwitchFrame || (frame_type == kKeyFrame && show_frame))
                || r.f(1) != 0;
        }
        const bool intra = frame_type == kIntraOnlyFrame || frame_type == kKeyFrame;
        const bool disable_cdf_update = r.f(1) != 0;
        const int allow_screen_content_tools = s.seq_force_screen_content_tools == kSelect
            ? static_cast<int>(r.f(1))
            : s.seq_force_screen_content_tools;
        bool force_integer_mv = false;
        if (allow_screen_content_tools != 0) {
            force_integer_mv = s.seq_force_integer_mv == kSelect ? r.f(1) != 0 : s.seq_force_integer_mv != 0;
        }
        if (intra) {
            force_integer_mv = true;
        }
        if (s.frame_id_numbers_present) {
            (void)r.f(s.frame_id_length);  // current_frame_id
        }
        bool frame_size_override = false;
        if (frame_type == kSwitchFrame) {
            frame_size_override = true;
        } else if (!s.reduced_still_picture_header) {
            frame_size_override = r.f(1) != 0;
        }
        (void)r.f(s.order_hint_bits);  // order_hint
        if (!intra && !error_resilient_mode) {
            (void)r.f(3);  // primary_ref_frame
        }
        if (s.decoder_model_info_present && r.f(1) != 0) {  // buffer_removal_time_present_flag
            for (int op = 0; op < s.operating_points; ++op) {
                if (!s.decoder_model_present_for_op[op]) {
                    continue;
                }
                const int idc = s.operating_point_idc[op];
                const bool in_temporal = ((idc >> temporal_id) & 1) != 0;
                const bool in_spatial = ((idc >> (spatial_id + 8)) & 1) != 0;
                if (idc == 0 || (in_temporal && in_spatial)) {
                    (void)r.f(s.buffer_removal_time_length);
                }
            }
        }
        constexpr uint32_t kAllFrames = 0xFF;
        uint32_t refresh_frame_flags = kAllFrames;
        if (!(frame_type == kSwitchFrame || (frame_type == kKeyFrame && show_frame))) {
            refresh_frame_flags = r.f(8);
        }
        if ((!intra || refresh_frame_flags != kAllFrames) && error_resilient_mode && s.enable_order_hint) {
            for (int i = 0; i < 8; ++i) {
                (void)r.f(s.order_hint_bits);  // ref_order_hint[i]
            }
        }
        int mi_cols = 0;
        int mi_rows = 0;
        if (intra) {
            const bool use_superres = ReadFrameSize(r, s, frame_size_override, mi_cols, mi_rows);
            ReadRenderSize(r);
            if (allow_screen_content_tools != 0 && !use_superres) {
                (void)r.f(1);  // allow_intrabc
            }
        } else {
            bool short_signaling = false;
            if (s.enable_order_hint) {
                short_signaling = r.f(1) != 0;
                if (short_signaling) {
                    (void)r.f(3);  // last_frame_idx
                    (void)r.f(3);  // gold_frame_idx
                }
            }
            for (int i = 0; i < 7; ++i) {
                if (!short_signaling) {
                    (void)r.f(3);  // ref_frame_idx[i]
                }
                if (s.frame_id_numbers_present) {
                    (void)r.f(s.delta_frame_id_length);  // delta_frame_id_minus_1
                }
            }
            if (frame_size_override && !error_resilient_mode) {
                // frame_size_with_refs: a found_ref takes the size of a reference frame, which
                // only a decoder keeps. Without one the size is coded here as usual.
                for (int i = 0; i < 7; ++i) {
                    if (r.f(1) != 0) {
                        return std::nullopt;
                    }
                }
            }
            ReadFrameSize(r, s, frame_size_override, mi_cols, mi_rows);
            ReadRenderSize(r);
            if (!force_integer_mv) {
                (void)r.f(1);  // allow_high_precision_mv
            }
            if (r.f(1) == 0) {  // is_filter_switchable
                (void)r.f(2);   // interpolation_filter
            }
            (void)r.f(1);  // is_motion_mode_switchable
            if (!error_resilient_mode && s.enable_ref_frame_mvs) {
                (void)r.f(1);  // use_ref_frame_mvs
            }
        }
        if (!(s.reduced_still_picture_header || disable_cdf_update)) {
            (void)r.f(1);  // disable_frame_end_update_cdf
        }
        ReadTileInfo(r, s, mi_cols, mi_rows);
        const uint32_t base_q_idx = r.f(8);
        if (!r.ok()) {
            return std::nullopt;
        }
        return static_cast<int>(base_q_idx);
    }

    std::optional<Sequence> sequence_;
};

struct DecodedI420Frame {
    std::vector<uint8_t> data;
    int width = 0;
    int height = 0;
    uint32_t rtp_timestamp = 0;
    int64_t timestamp_us = 0;
};

#endif

OmniWebRtcMediaStatus CopyString(
    std::string_view value,
    char* buffer,
    size_t buffer_len,
    size_t* required_len) noexcept
{
    const size_t required = value.size() + 1;
    if (required_len != nullptr) {
        *required_len = required;
    }

    if (buffer == nullptr || buffer_len == 0) {
        return OMNI_WEBRTC_MEDIA_STATUS_BUFFER_TOO_SMALL;
    }

    const size_t copy_len = std::min(value.size(), buffer_len - 1);
    if (copy_len > 0) {
        std::memcpy(buffer, value.data(), copy_len);
    }

    buffer[copy_len] = '\0';
    return buffer_len >= required
        ? OMNI_WEBRTC_MEDIA_STATUS_OK
        : OMNI_WEBRTC_MEDIA_STATUS_BUFFER_TOO_SMALL;
}

std::string JsonEscape(std::string_view value)
{
    std::string escaped;
    escaped.reserve(value.size());
    for (const char ch : value) {
        switch (ch) {
        case '\\':
            escaped += "\\\\";
            break;
        case '"':
            escaped += "\\\"";
            break;
        case '\b':
            escaped += "\\b";
            break;
        case '\f':
            escaped += "\\f";
            break;
        case '\n':
            escaped += "\\n";
            break;
        case '\r':
            escaped += "\\r";
            break;
        case '\t':
            escaped += "\\t";
            break;
        default:
            if (static_cast<unsigned char>(ch) < 0x20) {
                escaped += "\\u00";
                constexpr char kHex[] = "0123456789abcdef";
                escaped += kHex[(static_cast<unsigned char>(ch) >> 4) & 0x0F];
                escaped += kHex[static_cast<unsigned char>(ch) & 0x0F];
            } else {
                escaped += ch;
            }
            break;
        }
    }
    return escaped;
}

std::string JsonArray(const std::set<std::string>& values)
{
    std::ostringstream json;
    json << '[';

    bool first = true;
    for (const std::string& value : values) {
        if (!first) {
            json << ',';
        }

        first = false;
        json << '"' << JsonEscape(value) << '"';
    }

    json << ']';
    return json.str();
}

std::string JsonArray(const std::vector<std::string>& values)
{
    std::ostringstream json;
    json << '[';

    bool first = true;
    for (const std::string& value : values) {
        if (!first) {
            json << ',';
        }

        first = false;
        json << '"' << JsonEscape(value) << '"';
    }

    json << ']';
    return json.str();
}

std::string LowerAscii(std::string_view value)
{
    std::string lowered;
    lowered.reserve(value.size());
    for (const char ch : value) {
        lowered.push_back(static_cast<char>(std::tolower(static_cast<unsigned char>(ch))));
    }
    return lowered;
}

/// Различить RTP и RTCP в одной трубе. Второй байт у RTCP — это его тип пакета, и он лежит в
/// 192..223 (RFC 5761, раздел 4); у RTP на этом месте маркер и тип нагрузки, а наши типы нагрузки
/// далеко за пределами этого диапазона. Проверка стандартная, а не наша выдумка: именно так
/// разделяют совмещённые потоки все, кто их совмещает.
inline bool LooksLikeRtcp(const uint8_t* packet, size_t packet_len)
{
    if (packet == nullptr || packet_len < 4) {
        return false;
    }
    const uint8_t payload_type = packet[1] & 0x7Fu;
    return payload_type >= 64u && payload_type <= 95u;
}

#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
void CopyPlane(
    uint8_t* destination,
    int destination_stride,
    const uint8_t* source,
    int source_stride,
    int width,
    int height);

class SmokeEncodeCallback final : public webrtc::EncodedImageCallback {
public:
    Result OnEncodedImage(
        const webrtc::EncodedImage& encoded_image,
        const webrtc::CodecSpecificInfo* codec_specific_info) override
    {
        (void)codec_specific_info;

        const uint8_t* data = encoded_image.data();
        const size_t size = encoded_image.size();
        if (data != nullptr && size > 0) {
            std::lock_guard<std::mutex> lock(mutex);
            encoded_data.assign(data, data + size);
            frame_type = encoded_image.FrameType();
            frames.push_back({encoded_data, encoded_image.qp_});
            ready = true;
            condition.notify_all();
        }

        return Result(Result::OK);
    }

    bool WaitForFrame(std::chrono::milliseconds timeout)
    {
        std::unique_lock<std::mutex> lock(mutex);
        return condition.wait_for(lock, timeout, [&] { return ready; });
    }

    std::vector<uint8_t> encoded_data;
    webrtc::VideoFrameType frame_type = webrtc::VideoFrameType::kVideoFrameDelta;
    // Every frame in order with the QP the encoder reported for it.
    std::vector<std::pair<std::vector<uint8_t>, int>> frames;

    size_t FrameCount()
    {
        std::lock_guard<std::mutex> lock(mutex);
        return frames.size();
    }

private:
    std::mutex mutex;
    std::condition_variable condition;
    bool ready = false;
};

class SmokeDecodeCallback final : public webrtc::DecodedImageCallback {
public:
    int32_t Decoded(webrtc::VideoFrame& decodedImage) override
    {
        std::lock_guard<std::mutex> lock(mutex);
        decoded_i420 = decodedImage.video_frame_buffer()->ToI420();
        ready = true;
        condition.notify_all();
        return WEBRTC_VIDEO_CODEC_OK;
    }

    void Decoded(
        webrtc::VideoFrame& decodedImage,
        std::optional<int32_t> decode_time_ms,
        std::optional<uint8_t> qp) override
    {
        (void)decode_time_ms;
        (void)qp;
        std::lock_guard<std::mutex> lock(mutex);
        decoded_i420 = decodedImage.video_frame_buffer()->ToI420();
        ready = true;
        condition.notify_all();
    }

    bool WaitForFrame(std::chrono::milliseconds timeout)
    {
        std::unique_lock<std::mutex> lock(mutex);
        return condition.wait_for(lock, timeout, [&] { return ready; });
    }

    webrtc::scoped_refptr<webrtc::I420BufferInterface> decoded_i420;

private:
    std::mutex mutex;
    std::condition_variable condition;
    bool ready = false;
};

class SessionEncodeCallback final : public webrtc::EncodedImageCallback {
public:
    Result OnEncodedImage(
        const webrtc::EncodedImage& encoded_image,
        const webrtc::CodecSpecificInfo* codec_specific_info) override
    {
        (void)codec_specific_info;

        const uint8_t* data = encoded_image.data();
        const size_t size = encoded_image.size();
        if (data == nullptr || size == 0) {
            return Result(Result::OK);
        }

        EncodedFrame frame;
        frame.data.assign(data, data + size);
        frame.frame_type = encoded_image.FrameType();
        frame.rtp_timestamp = encoded_image.RtpTimestamp();
        // Wave 6 (2026-05-18): pass encoder-reported QP to the caller for
        // QualityScaler::ReportQp(). EncodedImage::qp_ is encoder-private but
        // exposed via the field directly (no accessor). Values < 0 mean
        // "not reported" — we map to nullopt so the scaler skips this sample.
        if (encoded_image.qp_ >= 0) {
            frame.qp = static_cast<uint8_t>(encoded_image.qp_);
        }

        {
            std::lock_guard<std::mutex> lock(mutex_);
            if (frames_.size() >= kMaxEncodedFrames) {
                frames_.pop_front();
                ++dropped_frames_;
            }

            frames_.push_back(std::move(frame));
        }

        condition_.notify_all();
        return Result(Result::OK);
    }

    /// Забрать результат, если он уже есть, не блокируясь.
    ///
    /// Кодировщики libwebrtc вызывают callback СИНХРОННО из Encode(), поэтому пустая очередь сразу
    /// после возврата Encode() означает не «ещё не готово», а «этот кадр сознательно пропущен» —
    /// нормальный режим с тех пор, как кодировщику разрешён пропуск кадров (SetFrameDropEnabled).
    bool TryPop(EncodedFrame& frame)
    {
        std::lock_guard<std::mutex> lock(mutex_);
        if (frames_.empty()) {
            return false;
        }

        frame = std::move(frames_.front());
        frames_.pop_front();
        return true;
    }

    bool WaitAndPop(std::chrono::milliseconds timeout, EncodedFrame& frame)
    {
        std::unique_lock<std::mutex> lock(mutex_);
        if (!condition_.wait_for(lock, timeout, [&] { return !frames_.empty(); })) {
            return false;
        }

        frame = std::move(frames_.front());
        frames_.pop_front();
        return true;
    }

    size_t dropped_frames() const
    {
        std::lock_guard<std::mutex> lock(mutex_);
        return dropped_frames_;
    }

private:
    static constexpr size_t kMaxEncodedFrames = 4;

    mutable std::mutex mutex_;
    std::condition_variable condition_;
    std::deque<EncodedFrame> frames_;
    size_t dropped_frames_ = 0;
};

// Wave 6 (2026-05-18): null sink for OveruseFrameDetector metrics. The
// detector demands a metrics observer to call OnEncodedFrameTimeMeasured
// after every CheckForOveruse tick; we don't need those numbers anywhere
// (we only react to AdaptUp/Down from OveruseFrameDetectorObserverInterface),
// so we discard. Atomically counts ticks so smoke tests can verify the
// detector is actually running.
/// Часы, по которым живёт всё наше время. Объявлены здесь, потому что нужны выше по файлу,
/// чем определены; определение — ниже, вместе с виртуальными часами стендов (#59).
int64_t OmniNowMs();

class NullCpuOveruseMetricsObserver final : public webrtc::CpuOveruseMetricsObserver {
public:
    void OnEncodedFrameTimeMeasured(int /*encode_duration_ms*/,
                                    int /*encode_usage_percent*/) override
    {
        ticks_.fetch_add(1, std::memory_order_relaxed);
    }

    uint64_t ticks() const { return ticks_.load(std::memory_order_relaxed); }

private:
    std::atomic<uint64_t> ticks_{0};
};

// СТАТИСТИКА КОДИРУЮЩЕЙ СТОРОНЫ — ОБЯЗАТЕЛЬНЫЙ СОБЕСЕДНИК VideoStreamEncoder (#61, шаг D2b).
//
// Оркестратор кодирования не хранит о себе ничего: всё, что он знает — сколько кадров пришло,
// сколько выброшено и почему, на какой кодировщик он переключился, как менял разрешение и частоту —
// он отдаёт сюда. У апстрима на этом конце стоит SendStatisticsProxy; нам нужны те же величины, но
// в нашей диагностике, поэтому здесь счётчики, а не вся его машинерия.
//
// GetInputFrameRate — единственный метод, который НЕ наблюдение, а вопрос: адаптация по процессору
// спрашивает у нас частоту входящих кадров. Считаем её по приходу, скользящим окном в секунду.
class OmniEncoderStatsObserver final : public webrtc::VideoStreamEncoderObserver {
public:
    void OnIncomingFrame(int width, int height) override
    {
        incoming_frames_.fetch_add(1, std::memory_order_relaxed);
        incoming_width_.store(width, std::memory_order_relaxed);
        incoming_height_.store(height, std::memory_order_relaxed);
        std::lock_guard<std::mutex> lock(mutex_);
        const int64_t now_ms = OmniNowMs();
        incoming_times_ms_.push_back(now_ms);
        while (!incoming_times_ms_.empty() && now_ms - incoming_times_ms_.front() > 1000) {
            incoming_times_ms_.pop_front();
        }
    }

    void OnEncodedFrameTimeMeasured(int encode_duration_ms, int encode_usage_percent) override
    {
        encode_duration_ms_.store(encode_duration_ms, std::memory_order_relaxed);
        encode_usage_percent_.store(encode_usage_percent, std::memory_order_relaxed);
        overuse_ticks_.fetch_add(1, std::memory_order_relaxed);
    }

    void OnSendEncodedImage(
        const webrtc::EncodedImage& encoded_image,
        const webrtc::CodecSpecificInfo* /*codec_info*/) override
    {
        encoded_frames_.fetch_add(1, std::memory_order_relaxed);
        if (encoded_image.FrameType() == webrtc::VideoFrameType::kVideoFrameKey) {
            encoded_keyframes_.fetch_add(1, std::memory_order_relaxed);
        }
    }

    void OnEncoderImplementationChanged(webrtc::EncoderImplementation implementation) override
    {
        std::lock_guard<std::mutex> lock(mutex_);
        implementation_name_ = implementation.name;
        implementation_is_hardware_ = implementation.is_hardware_accelerated;
        ++implementation_changes_;
    }

    void OnFrameDropped(DropReason reason) override
    {
        dropped_frames_.fetch_add(1, std::memory_order_relaxed);
        switch (reason) {
            case DropReason::kEncoderQueue:
                dropped_by_queue_.fetch_add(1, std::memory_order_relaxed);
                break;
            case DropReason::kEncoder:
                dropped_by_encoder_.fetch_add(1, std::memory_order_relaxed);
                break;
            case DropReason::kMediaOptimization:
                dropped_by_rate_.fetch_add(1, std::memory_order_relaxed);
                break;
            default:
                break;
        }
    }

    void OnEncoderReconfigured(
        const webrtc::VideoEncoderConfig& /*encoder_config*/,
        const std::vector<webrtc::VideoStream>& /*streams*/) override
    {
        reconfigurations_.fetch_add(1, std::memory_order_relaxed);
    }

    void OnAdaptationChanged(
        webrtc::VideoAdaptationReason /*reason*/,
        const webrtc::VideoAdaptationCounters& cpu_steps,
        const webrtc::VideoAdaptationCounters& quality_steps) override
    {
        adaptation_changes_.fetch_add(1, std::memory_order_relaxed);
        cpu_resolution_steps_.store(cpu_steps.resolution_adaptations,
                                    std::memory_order_relaxed);
        cpu_framerate_steps_.store(cpu_steps.fps_adaptations, std::memory_order_relaxed);
        quality_resolution_steps_.store(quality_steps.resolution_adaptations,
                                        std::memory_order_relaxed);
        quality_framerate_steps_.store(quality_steps.fps_adaptations,
                                       std::memory_order_relaxed);
    }

    void ClearAdaptationStats() override
    {
        cpu_resolution_steps_.store(0, std::memory_order_relaxed);
        cpu_framerate_steps_.store(0, std::memory_order_relaxed);
        quality_resolution_steps_.store(0, std::memory_order_relaxed);
        quality_framerate_steps_.store(0, std::memory_order_relaxed);
    }

    /// ВКЛЮЧЕНЫ ЛИ ОСИ АДАПТАЦИИ — ГОВОРИТ САМ ОРКЕСТРАТОР, А НЕ НАША ДОГАДКА.
    ///
    /// Был пустой заглушкой, и зря: это единственное место, где апстрим прямо сообщает, считает
    /// ли он каждую ось рабочей. Без него «шагов адаптации ноль» одинаково читается и как
    /// «механизм работает и не считает нужным», и как «механизм не заведён» — а это разные
    /// починки. Живой звонок 2026-08-27 стоял на нуле сорок две секунды, и различить их было
    /// нечем (docs/bugs/resolution-adapts-in-the-stand-not-in-the-app.md).
    void UpdateAdaptationSettings(
        AdaptationSettings cpu_settings,
        AdaptationSettings quality_settings) override
    {
        cpu_resolution_scaling_.store(cpu_settings.resolution_scaling_enabled,
                                      std::memory_order_relaxed);
        cpu_framerate_scaling_.store(cpu_settings.framerate_scaling_enabled,
                                     std::memory_order_relaxed);
        quality_resolution_scaling_.store(quality_settings.resolution_scaling_enabled,
                                          std::memory_order_relaxed);
        quality_framerate_scaling_.store(quality_settings.framerate_scaling_enabled,
                                         std::memory_order_relaxed);
        adaptation_settings_updates_.fetch_add(1, std::memory_order_relaxed);
    }

    bool cpu_resolution_scaling() const
    {
        return cpu_resolution_scaling_.load(std::memory_order_relaxed);
    }
    bool quality_resolution_scaling() const
    {
        return quality_resolution_scaling_.load(std::memory_order_relaxed);
    }
    bool quality_framerate_scaling() const
    {
        return quality_framerate_scaling_.load(std::memory_order_relaxed);
    }
    uint64_t adaptation_settings_updates() const
    {
        return adaptation_settings_updates_.load(std::memory_order_relaxed);
    }

    void OnMinPixelLimitReached() override
    {
        min_pixel_limit_reached_.fetch_add(1, std::memory_order_relaxed);
    }

    void OnInitialQualityResolutionAdaptDown() override
    {
        initial_quality_adapt_downs_.fetch_add(1, std::memory_order_relaxed);
    }

    void OnSuspendChange(bool is_suspended) override
    {
        suspended_.store(is_suspended, std::memory_order_relaxed);
    }

    int GetInputFrameRate() const override
    {
        std::lock_guard<std::mutex> lock(mutex_);
        return static_cast<int>(incoming_times_ms_.size());
    }

    uint64_t incoming_frames() const { return incoming_frames_.load(std::memory_order_relaxed); }
    uint64_t encoded_frames() const { return encoded_frames_.load(std::memory_order_relaxed); }
    uint64_t encoded_keyframes() const { return encoded_keyframes_.load(std::memory_order_relaxed); }
    uint64_t dropped_frames() const { return dropped_frames_.load(std::memory_order_relaxed); }
    uint64_t dropped_by_queue() const { return dropped_by_queue_.load(std::memory_order_relaxed); }
    uint64_t dropped_by_encoder() const { return dropped_by_encoder_.load(std::memory_order_relaxed); }
    uint64_t dropped_by_rate() const { return dropped_by_rate_.load(std::memory_order_relaxed); }
    uint64_t reconfigurations() const { return reconfigurations_.load(std::memory_order_relaxed); }
    uint64_t adaptation_changes() const { return adaptation_changes_.load(std::memory_order_relaxed); }
    uint64_t overuse_ticks() const { return overuse_ticks_.load(std::memory_order_relaxed); }
    int cpu_resolution_steps() const { return cpu_resolution_steps_.load(std::memory_order_relaxed); }
    int cpu_framerate_steps() const { return cpu_framerate_steps_.load(std::memory_order_relaxed); }
    int quality_resolution_steps() const { return quality_resolution_steps_.load(std::memory_order_relaxed); }
    int quality_framerate_steps() const { return quality_framerate_steps_.load(std::memory_order_relaxed); }
    int encode_usage_percent() const { return encode_usage_percent_.load(std::memory_order_relaxed); }
    int encode_duration_ms() const { return encode_duration_ms_.load(std::memory_order_relaxed); }
    bool suspended() const { return suspended_.load(std::memory_order_relaxed); }

    std::string implementation_name() const
    {
        std::lock_guard<std::mutex> lock(mutex_);
        return implementation_name_;
    }

    bool implementation_is_hardware() const
    {
        std::lock_guard<std::mutex> lock(mutex_);
        return implementation_is_hardware_;
    }

private:
    mutable std::mutex mutex_;
    std::deque<int64_t> incoming_times_ms_;
    std::string implementation_name_;
    bool implementation_is_hardware_ = false;
    uint64_t implementation_changes_ = 0;

    std::atomic<uint64_t> incoming_frames_{0};
    std::atomic<uint64_t> encoded_frames_{0};
    std::atomic<uint64_t> encoded_keyframes_{0};
    std::atomic<uint64_t> dropped_frames_{0};
    std::atomic<uint64_t> dropped_by_queue_{0};
    std::atomic<uint64_t> dropped_by_encoder_{0};
    std::atomic<uint64_t> dropped_by_rate_{0};
    std::atomic<uint64_t> reconfigurations_{0};
    std::atomic<uint64_t> adaptation_changes_{0};
    std::atomic<uint64_t> overuse_ticks_{0};
    std::atomic<int> incoming_width_{0};
    std::atomic<int> incoming_height_{0};
    std::atomic<bool> cpu_resolution_scaling_{false};
    std::atomic<bool> cpu_framerate_scaling_{false};
    std::atomic<bool> quality_resolution_scaling_{false};
    std::atomic<bool> quality_framerate_scaling_{false};
    std::atomic<uint64_t> adaptation_settings_updates_{0};
    std::atomic<int> cpu_resolution_steps_{0};
    std::atomic<int> cpu_framerate_steps_{0};
    std::atomic<int> quality_resolution_steps_{0};
    std::atomic<int> quality_framerate_steps_{0};
    std::atomic<int> encode_usage_percent_{0};
    std::atomic<int> encode_duration_ms_{0};
    std::atomic<uint64_t> min_pixel_limit_reached_{0};
    std::atomic<uint64_t> initial_quality_adapt_downs_{0};
    std::atomic<bool> suspended_{false};
};

// ИСТОЧНИК КАДРОВ ДЛЯ VideoStreamEncoder (#61, шаг D2b).
//
// Апстрим ждёт источник, который сам отдаёт кадры и умеет слушать пожелания
// (`VideoSinkWants`: не больше стольких точек, не чаще стольких кадров). У нас кадры приносит
// оболочка через FFI, поэтому источник здесь — тонкий переходник: `PushFrame` отдаёт кадр
// подписчику, а пожелания запоминаются и читаются наружу.
//
// Пожелания — это и есть та величина, которую оболочка получает обратно как цель ABR: «дай кадр
// не больше такого-то размера и не чаще такой-то частоты». Апстрим адресует их источнику захвата,
// у нас источник захвата живёт в оболочке — значит их надо ей передать. Ровно тот же смысл, тот же
// адресат.
class OmniPushVideoSource final : public webrtc::VideoSourceInterface<webrtc::VideoFrame> {
public:
    void AddOrUpdateSink(
        webrtc::VideoSinkInterface<webrtc::VideoFrame>* sink,
        const webrtc::VideoSinkWants& wants) override
    {
        std::lock_guard<std::mutex> lock(mutex_);
        sink_ = sink;
        // ПЕРЕХОДЫ «СУЗИЛ / ОТПУСТИЛ» СЧИТАЮТСЯ ЗДЕСЬ, А НЕ ВЫВОДЯТСЯ ИЗ ОПРОСА.
        //
        // Живой звонок показал размер, который опустился и вернулся за 262 мс, — а такт ABR
        // опрашивает пожелания 3-4 раза в секунду и по одним лишь снимкам нельзя отличить
        // «оркестратор передумал» от «мы поймали мгновение». Считаем сам факт смены в точке, где
        // она происходит.
        const int previous_max_pixels = wants_.max_pixel_count;
        wants_ = wants;
        if (wants.max_pixel_count < previous_max_pixels) {
            ++wants_narrowed_;
        } else if (wants.max_pixel_count > previous_max_pixels) {
            ++wants_widened_;
        }
        ++wants_updates_;
    }

    void RemoveSink(webrtc::VideoSinkInterface<webrtc::VideoFrame>* sink) override
    {
        std::lock_guard<std::mutex> lock(mutex_);
        if (sink_ == sink) {
            sink_ = nullptr;
        }
    }

    /// Отдать кадр подписчику. Возвращает false, если подписчика нет.
    bool PushFrame(const webrtc::VideoFrame& frame)
    {
        webrtc::VideoSinkInterface<webrtc::VideoFrame>* sink = nullptr;
        {
            std::lock_guard<std::mutex> lock(mutex_);
            sink = sink_;
        }
        if (sink == nullptr) {
            return false;
        }
        sink->OnFrame(frame);
        return true;
    }

    /// Пожелания оркестратора: сколько точек и сколько кадров в секунду он готов принять.
    webrtc::VideoSinkWants wants() const
    {
        std::lock_guard<std::mutex> lock(mutex_);
        return wants_;
    }

    uint64_t wants_updates() const
    {
        std::lock_guard<std::mutex> lock(mutex_);
        return wants_updates_;
    }

    /// Сколько раз оркестратор СУЗИЛ пожелания и сколько раз ОТПУСТИЛ.
    uint64_t wants_narrowed() const
    {
        std::lock_guard<std::mutex> lock(mutex_);
        return wants_narrowed_;
    }

    uint64_t wants_widened() const
    {
        std::lock_guard<std::mutex> lock(mutex_);
        return wants_widened_;
    }

private:
    mutable std::mutex mutex_;
    webrtc::VideoSinkInterface<webrtc::VideoFrame>* sink_ = nullptr;
    webrtc::VideoSinkWants wants_;
    uint64_t wants_updates_ = 0;
    uint64_t wants_narrowed_ = 0;
    uint64_t wants_widened_ = 0;
};

// НАШ ОБЪЕДИНЁННЫЙ НАБЛЮДАТЕЛЬ АДАПТАЦИИ УБРАН (#61, шаг D3).
//
// Он собирал сигналы двух детекторов — перегрузки процессора и роста QP — в четыре счётчика,
// по которым такт ABR двигал разрешение и частоту. Обоими детекторами теперь владеет
// VideoStreamEncoder, а переводит их сигналы в решение VideoStreamEncoderResourceManager. Наши
// счётчики после переворота никто не наполнял: диагностика adapt_cpu_*/adapt_qp_* показывала
// ровные нули и читалась как «детекторы молчат», хотя означала «нам их больше не показывают».
// Взамен видно то, что считает сам оркестратор: шаги адаптации отдельно по процессору и по
// качеству (VideoStreamEncoderObserver::OnAdaptationChanged) и его пожелания источнику.

// ПРОСЬБЫ ПРИЁМНИКА УХОДЯТ НАСТОЯЩИМ RTCP (шаг A2, 2026-10-02).
//
// У апстрима их отправляет сам приёмный поток: RtpVideoStreamReceiver2 реализует NackSender и
// KeyFrameRequestSender, и обе просьбы уходят его RTCP-модулем (rtp_rtcp_->SendNack,
// SendPictureLossIndication). У нас приёмник — половина сессии, поэтому здесь переходник с двумя
// обработчиками, а уходят просьбы тем же приёмным модулем, что и отчёты.
//
// Раньше номера копились здесь и забирались оболочкой раз в 50 мс структурным событием через ядро;
// на той стороне оболочка своим тактом отдавала их отправителю. Просьба о повторе стояла в двух
// очередях по полтакта каждая — при пути ~50 мс повтор часто приходил, когда кадр уже пора было
// показывать. См. docs/bugs/media-rode-the-shells-timer.md, шаг 3.
class OmniReceiverRtcpFeedback final : public webrtc::NackSender,
                                       public webrtc::KeyFrameRequestSender {
public:
    using NackHandler = std::function<void(const std::vector<uint16_t>&)>;
    using KeyframeHandler = std::function<void()>;
    OmniReceiverRtcpFeedback(NackHandler on_nack, KeyframeHandler on_keyframe)
        : on_nack_(std::move(on_nack)), on_keyframe_(std::move(on_keyframe)) {}

    // `buffering_allowed` — подсказка «можно склеить с другой обратной связью». Апстрим без
    // буфера обратной связи её тоже не читает: RTCPSender и так кладёт NACK в составной пакет с
    // отчётом.
    void SendNack(const std::vector<uint16_t>& sequence_numbers,
                  bool /*buffering_allowed*/) override
    {
        if (!sequence_numbers.empty()) {
            on_nack_(sequence_numbers);
        }
    }

    void RequestKeyFrame() override { on_keyframe_(); }

private:
    NackHandler on_nack_;
    KeyframeHandler on_keyframe_;
};

// Счётчики RTCP, как их видит сам модуль (RTCPSender — отправленное, RTCPReceiver — принятое).
// Апстрим собирает ими статистику потока (SendStatisticsProxy); нам они нужны для диагностики:
// сколько просьб о повторе и ключевом кадре ушло на провод и сколько их пришло.
//
// Модуль сообщает накопленное, а не приращение. Сообщают ОБЕ его половины через одного наблюдателя:
// RTCPSender — отправленное (под SSRC собеседника), RTCPReceiver — принятое (под своим медийным
// SSRC, а у приёмного модуля это тоже SSRC собеседника — recv_ssrc_callback). Различить их по SSRC
// нельзя, и принимающая половина приёмного модуля присылает нули поверх отправленных NACK — замер
// поймал это сразу, как только приёмному модулю пошли SR отправителя. Каждое поле накапливается
// только в одной из половин (приёмный модуль NACK не получает, отправитель видео его не шлёт),
// поэтому хранится наибольшее из сообщённого. Чужой SSRC (RTX) отсеивается.
// RTT ПРИЁМНИКА — СВОЙ, ИЗ XR (RFC 3611: RRTR -> DLRR), КАК У АПСТРИМА ДЛЯ ПОТОКА БЕЗ ОТПРАВКИ.
//
// Приёмная сессия медиа не шлёт, и из обычного RR ей RTT не вычислить. Раньше его возила оболочка:
// каждые 50 мс брала RTT, измеренный отправляющей сессией по отчётам о доставке, и отдавала
// приёмной. У апстрима на этот случай есть штатный механизм: модуль с non_sender_rtt_measurement
// кладёт в свои отчёты RRTR, собеседник отвечает DLRR, модуль считает RTT и раз в секунду отдаёт
// его наблюдателю RtcpRttStats (ModuleRtpRtcpImpl2::PeriodicUpdate). Наблюдатель у апстрима —
// CallStats, а он передаёт RTT в VideoReceiveStream2::OnRttUpdate: запросчику NACK и буферу показа.
class OmniRttObserver final : public webrtc::RtcpRttStats {
public:
    explicit OmniRttObserver(std::function<void(int64_t)> on_rtt) : on_rtt_(std::move(on_rtt)) {}
    void OnRttUpdate(int64_t rtt_ms) override { on_rtt_(rtt_ms); }

private:
    std::function<void(int64_t)> on_rtt_;
};

// ОТЧЁТЫ СОБЕСЕДНИКА О НАШЕМ ПОТОКЕ (RR) — ОТ САМОГО ОТПРАВИТЕЛЯ ВИДЕО (ABI 18).
//
// До этого их нам пересказывала оболочка: приёмник собеседника собирал структурное событие,
// управляющий канал вёз его к нам, оболочка запоминала потери/джиттер и МОМЕНТ прихода и отдавала
// всё это в такт ABR. С ABI 16–17 настоящий RR приходит отправителю видео байтами RTCP; модуль сам
// его разбирает и сообщает каждый блок наблюдателю (у апстрима — SendStatisticsProxy).
// Нужно это трём потребителям: диагностике (потери/джиттер/RTT у собеседника), тренду ABR и
// давности обратной связи (см. kAbrFeedbackLostMs).
class OmniReportBlockObserver final : public webrtc::ReportBlockDataObserver {
public:
    explicit OmniReportBlockObserver(uint32_t media_ssrc) : media_ssrc_(media_ssrc) {}

    void OnReportBlockDataUpdated(webrtc::ReportBlockData data) override
    {
        if (data.source_ssrc() != media_ssrc_) {
            return;
        }
        fraction_lost_q8_.store(data.fraction_lost_raw(), std::memory_order_relaxed);
        jitter_.store(data.jitter(), std::memory_order_relaxed);
        if (data.has_rtt()) {
            rtt_ms_.store(data.last_rtt().ms(), std::memory_order_relaxed);
        }
        reports_.fetch_add(1, std::memory_order_relaxed);
    }

    uint64_t reports() const { return reports_.load(std::memory_order_relaxed); }
    uint8_t fraction_lost_q8() const { return fraction_lost_q8_.load(std::memory_order_relaxed); }
    uint32_t jitter() const { return jitter_.load(std::memory_order_relaxed); }
    /// -1 — ещё не вычислен (нужен наш SR, на который сошлётся RR).
    int64_t rtt_ms() const { return rtt_ms_.load(std::memory_order_relaxed); }

private:
    const uint32_t media_ssrc_;
    std::atomic<uint64_t> reports_{0};
    std::atomic<uint8_t> fraction_lost_q8_{0};
    std::atomic<uint32_t> jitter_{0};
    std::atomic<int64_t> rtt_ms_{-1};
};

class OmniRtcpTypeCounter final : public webrtc::RtcpPacketTypeCounterObserver {
public:
    explicit OmniRtcpTypeCounter(uint32_t ssrc) : ssrc_(ssrc) {}

    void RtcpPacketTypesCounterUpdated(uint32_t ssrc,
                                       const webrtc::RtcpPacketTypeCounter& counter) override
    {
        if (ssrc != ssrc_) {
            return;
        }
        StoreMax(nack_packets_, counter.nack_packets);
        StoreMax(nack_requests_, counter.nack_requests);
        StoreMax(pli_packets_, counter.pli_packets);
    }

    uint32_t nack_packets() const { return nack_packets_.load(std::memory_order_relaxed); }
    uint32_t nack_requests() const { return nack_requests_.load(std::memory_order_relaxed); }
    uint32_t pli_packets() const { return pli_packets_.load(std::memory_order_relaxed); }

private:
    static void StoreMax(std::atomic<uint32_t>& slot, uint32_t value)
    {
        uint32_t seen = slot.load(std::memory_order_relaxed);
        while (value > seen && !slot.compare_exchange_weak(seen, value, std::memory_order_relaxed)) {
        }
    }

    const uint32_t ssrc_;
    std::atomic<uint32_t> nack_packets_{0};
    std::atomic<uint32_t> nack_requests_{0};
    std::atomic<uint32_t> pli_packets_{0};
};

class SessionDecodeCallback final : public webrtc::DecodedImageCallback {
public:
    int32_t Decoded(webrtc::VideoFrame& decodedImage) override
    {
        Store(decodedImage);
        return WEBRTC_VIDEO_CODEC_OK;
    }

    void Decoded(
        webrtc::VideoFrame& decodedImage,
        std::optional<int32_t> decode_time_ms,
        std::optional<uint8_t> qp) override
    {
        (void)decode_time_ms;
        (void)qp;
        Store(decodedImage);
    }

    bool WaitForFrame(std::chrono::milliseconds timeout)
    {
        std::unique_lock<std::mutex> lock(mutex_);
        return condition_.wait_for(lock, timeout, [&] { return !frames_.empty(); });
    }

    bool Peek(DecodedI420Frame& frame) const
    {
        std::lock_guard<std::mutex> lock(mutex_);
        if (frames_.empty()) {
            return false;
        }

        frame = frames_.front();
        return true;
    }

    void Pop()
    {
        std::lock_guard<std::mutex> lock(mutex_);
        if (frames_.empty()) {
            return;
        }

        queued_bytes_ -= frames_.front().data.size();
        frames_.pop_front();
    }

    bool TryPop(DecodedI420Frame& frame)
    {
        if (!Peek(frame)) {
            return false;
        }

        Pop();
        return true;
    }

    size_t dropped_frames() const
    {
        std::lock_guard<std::mutex> lock(mutex_);
        return dropped_frames_;
    }

    size_t queued_frames() const
    {
        std::lock_guard<std::mutex> lock(mutex_);
        return frames_.size();
    }

private:
    static constexpr size_t kMaxFrames = 4;
    static constexpr size_t kMaxQueuedBytes = 32u * 1024u * 1024u;

    void Store(webrtc::VideoFrame& decoded_image)
    {
        webrtc::scoped_refptr<webrtc::I420BufferInterface> i420 =
            decoded_image.video_frame_buffer()->ToI420();
        if (!i420) {
            return;
        }

        DecodedI420Frame frame;
        frame.width = i420->width();
        frame.height = i420->height();
        frame.rtp_timestamp = decoded_image.rtp_timestamp();
        frame.timestamp_us = decoded_image.timestamp_us();

        const int chroma_width = (frame.width + 1) / 2;
        const int chroma_height = (frame.height + 1) / 2;
        const size_t y_size = static_cast<size_t>(frame.width) * static_cast<size_t>(frame.height);
        const size_t uv_size = static_cast<size_t>(chroma_width) * static_cast<size_t>(chroma_height);
        frame.data.resize(y_size + (uv_size * 2u));

        uint8_t* y_plane = frame.data.data();
        uint8_t* u_plane = y_plane + y_size;
        uint8_t* v_plane = u_plane + uv_size;
        CopyPlane(y_plane, frame.width, i420->DataY(), i420->StrideY(), frame.width, frame.height);
        CopyPlane(u_plane, chroma_width, i420->DataU(), i420->StrideU(), chroma_width, chroma_height);
        CopyPlane(v_plane, chroma_width, i420->DataV(), i420->StrideV(), chroma_width, chroma_height);

        {
            std::lock_guard<std::mutex> lock(mutex_);
            while (!frames_.empty() &&
                   (frames_.size() >= kMaxFrames || queued_bytes_ + frame.data.size() > kMaxQueuedBytes)) {
                queued_bytes_ -= frames_.front().data.size();
                frames_.pop_front();
                ++dropped_frames_;
            }

            if (frame.data.size() > kMaxQueuedBytes) {
                ++dropped_frames_;
                return;
            }

            queued_bytes_ += frame.data.size();
            frames_.push_back(std::move(frame));
        }

        condition_.notify_all();
    }

    mutable std::mutex mutex_;
    std::condition_variable condition_;
    std::deque<DecodedI420Frame> frames_;
    size_t queued_bytes_ = 0;
    size_t dropped_frames_ = 0;
};

std::set<std::string> CollectCodecNames(const std::vector<webrtc::SdpVideoFormat>& formats)
{
    std::set<std::string> codecs;
    for (const webrtc::SdpVideoFormat& format : formats) {
        if (format.name.empty()) {
            continue;
        }
        const std::string lowered = LowerAscii(format.name);
        // Wave 1 (2026-05-16): VP8 was removed from the production codec
        // set in Omni. libwebrtc still builds with libvpx, so the factory
        // continues to advertise VP8 — but we hide it from the probe so
        // negotiation never tries to pick it.
        if (lowered == "vp8") {
            continue;
        }
        codecs.insert(lowered);
    }
    return codecs;
}

std::set<std::string> IntersectCodecs(
    const std::set<std::string>& encoder_codecs,
    const std::set<std::string>& decoder_codecs)
{
    std::set<std::string> intersection;
    std::set_intersection(
        encoder_codecs.begin(),
        encoder_codecs.end(),
        decoder_codecs.begin(),
        decoder_codecs.end(),
        std::inserter(intersection, intersection.begin()));
    return intersection;
}

void FillPlane(uint8_t* data, int stride, int width, int height, uint8_t value)
{
    for (int y = 0; y < height; ++y) {
        std::memset(data + (y * stride), value, static_cast<size_t>(width));
    }
}

void CopyPlane(
    uint8_t* destination,
    int destination_stride,
    const uint8_t* source,
    int source_stride,
    int width,
    int height)
{
    for (int y = 0; y < height; ++y) {
        std::memcpy(
            destination + (static_cast<size_t>(y) * static_cast<size_t>(destination_stride)),
            source + (static_cast<size_t>(y) * static_cast<size_t>(source_stride)),
            static_cast<size_t>(width));
    }
}

void FillVerticalI420Bars(const webrtc::scoped_refptr<webrtc::I420Buffer>& buffer)
{
    const int width = buffer->width();
    const int height = buffer->height();
    for (int y = 0; y < height; ++y) {
        uint8_t* row = buffer->MutableDataY() + (y * buffer->StrideY());
        for (int x = 0; x < width; ++x) {
            row[x] = x < width / 3 ? 76 : x < (width * 2 / 3) ? 149 : 29;
        }
    }

    const int chroma_width = (width + 1) / 2;
    const int chroma_height = (height + 1) / 2;
    FillPlane(buffer->MutableDataU(), buffer->StrideU(), chroma_width, chroma_height, 128);
    FillPlane(buffer->MutableDataV(), buffer->StrideV(), chroma_width, chroma_height, 128);

    for (int y = 0; y < chroma_height; ++y) {
        uint8_t* row_u = buffer->MutableDataU() + (y * buffer->StrideU());
        uint8_t* row_v = buffer->MutableDataV() + (y * buffer->StrideV());
        for (int x = 0; x < chroma_width; ++x) {
            if (x < chroma_width / 3) {
                row_u[x] = 84;
                row_v[x] = 255;
            } else if (x < (chroma_width * 2 / 3)) {
                row_u[x] = 43;
                row_v[x] = 21;
            } else {
                row_u[x] = 255;
                row_v[x] = 107;
            }
        }
    }
}

double AverageLuma(
    const webrtc::scoped_refptr<webrtc::I420BufferInterface>& buffer,
    int start_x,
    int end_x)
{
    long long sum = 0;
    long long count = 0;
    for (int y = 0; y < buffer->height(); ++y) {
        const uint8_t* row = buffer->DataY() + (y * buffer->StrideY());
        for (int x = start_x; x < end_x; ++x) {
            sum += row[x];
            ++count;
        }
    }
    return count == 0 ? 0.0 : static_cast<double>(sum) / static_cast<double>(count);
}

bool DecodedColorBarsArePlausible(
    const webrtc::scoped_refptr<webrtc::I420BufferInterface>& decoded)
{
    if (!decoded || decoded->width() < 96 || decoded->height() < 64) {
        return false;
    }

    const int width = decoded->width();
    const double left = AverageLuma(decoded, width / 12, width / 4);
    const double middle = AverageLuma(decoded, (width * 5) / 12, (width * 7) / 12);
    const double right = AverageLuma(decoded, (width * 3) / 4, (width * 11) / 12);
    return middle > left + 20.0 && left > right + 20.0;
}

// Кодек одного направления звонка. Какой из них едет S -> R, решает ядро по паспортам обеих
// сторон (docs/decisions/per-pair-video-codec-selection.md); здесь только «как его везти».
// Типы нагрузки — та же таблица, что в ядре (default_video_payload_type).
enum class NativeVideoCodec {
    H264,
    H265,
    Av1,
};

std::string_view NativeVideoCodecName(NativeVideoCodec codec)
{
    switch (codec) {
    case NativeVideoCodec::Av1:
        return "av1";
    case NativeVideoCodec::H265:
        return "h265";
    case NativeVideoCodec::H264:
    default:
        return "h264";
    }
}

/// Имя для RtpConfig::payload_name: по нему RtpVideoSender выбирает упаковщик (PayloadStringToCodecType).
std::string_view NativeVideoCodecPayloadName(NativeVideoCodec codec)
{
    switch (codec) {
    case NativeVideoCodec::Av1:
        return "AV1";
    case NativeVideoCodec::H265:
        return "H265";
    case NativeVideoCodec::H264:
    default:
        return "H264";
    }
}

uint8_t NativeVideoCodecPayloadType(NativeVideoCodec codec)
{
    switch (codec) {
    case NativeVideoCodec::Av1:
        return 45;
    case NativeVideoCodec::H265:
        return 49;
    case NativeVideoCodec::H264:
    default:
        return 102;
    }
}

webrtc::VideoCodecType NativeVideoCodecType(NativeVideoCodec codec)
{
    switch (codec) {
    case NativeVideoCodec::Av1:
        return webrtc::kVideoCodecAV1;
    case NativeVideoCodec::H265:
        return webrtc::kVideoCodecH265;
    case NativeVideoCodec::H264:
    default:
        return webrtc::kVideoCodecH264;
    }
}

webrtc::RtpVideoFrameAssembler::PayloadFormat NativeVideoAssemblerFormat(NativeVideoCodec codec)
{
    switch (codec) {
    case NativeVideoCodec::Av1:
        return webrtc::RtpVideoFrameAssembler::kAv1;
    case NativeVideoCodec::H265:
        return webrtc::RtpVideoFrameAssembler::kH265;
    case NativeVideoCodec::H264:
    default:
        return webrtc::RtpVideoFrameAssembler::kH264;
    }
}

std::optional<NativeVideoCodec> ParseNativeVideoCodec(std::string_view codec)
{
    const std::string lowered = LowerAscii(codec);
    if (lowered.empty() || lowered == "h264") {
        return NativeVideoCodec::H264;
    }
    if (lowered == "av1") {
        return NativeVideoCodec::Av1;
    }
    if (lowered == "h265" || lowered == "hevc") {
        return NativeVideoCodec::H265;
    }

    return std::nullopt;
}

// Only AV1 has a software codec in the build (libaom encodes, dav1d decodes). H.264 and HEVC are
// patent-pool codecs: the build carries no OpenH264/FFmpeg (proprietary_codecs=false,
// rtc_use_h264=false), so they run on the Media Foundation hardware MFTs alone
// (docs/decisions/per-pair-video-codec-selection.md, "Без программного H.264").
bool HasSoftwareVideoCodec(NativeVideoCodec codec)
{
    return codec == NativeVideoCodec::Av1;
}

std::optional<webrtc::SdpVideoFormat> FindSupportedFormat(
    const std::vector<webrtc::SdpVideoFormat>& formats,
    NativeVideoCodec codec)
{
    const std::string wanted(NativeVideoCodecName(codec));
    for (const webrtc::SdpVideoFormat& format : formats) {
        if (LowerAscii(format.name) == wanted) {
            return format;
        }
    }

    return std::nullopt;
}

/// Сколько временных слоёв просим у кодировщика. ОДНО ЧИСЛО НА ВСЕХ.
///
/// Их три места: настройка программного кодека, настройка MFT и раскладка кадров по слоям, которую
/// мы отдаём корректору битрейта. Пока они были независимы, они и разошлись: слои включились на
/// обоих кодировщиках, а корректор продолжал считать, что слой один и ему достаются все кадры.
/// Замер 2026-08-17 показал это как «аппаратный путь теряет 10.7 дБ от временных слоёв». Вывод был
/// неверен дважды: рассинхронизация действительно была (и исправлена этой константой), но главная
/// ошибка сидела в постановке замера — сравнивалось качество при фиксированном ЗАПРОШЕННОМ
/// битрейте, которого одному слою на этой сцене уже некуда тратить. При РАВНОМ КАЧЕСТВЕ два слоя
/// стоят около 20% битрейта (6.78 против 5.62 пакета на кадр при 52.1 дБ у обоих) — обычная цена
/// временной масштабируемости, ради которой её и берут все.
///
/// Значение 1, и теперь это ЗАМЕРЕННОЕ решение, а не отложенное. Структура кадров передаётся
/// приёмнику расширением заголовка (см. VideoRtpExtensionMap), то есть выгоду получать УЖЕ ЕСТЬ
/// КОМУ. Замер под 10% потерь пакетов, с защищённым ключевым кадром, чтобы не мерить чужую
/// болезнь: один слой — 8 декодированных кадров и 37.0 дБ, два слоя — 8 кадров и 36.4. Разницы нет.
///
/// Причина понятная: при потере ПАКЕТОВ нижний слой уязвим ровно так же, как верхний, и цепочка
/// рвётся на нём. Временные слои в настоящих ВКС нужны прежде всего для SFU — он выбрасывает
/// верхний слой медленному получателю, не перекодируя, — и для намеренного снижения частоты кадров.
/// У нас звонки 1-на-1 и SFU нет, поэтому главная их польза к нам не относится, а 20% битрейта
/// они стоят по-настоящему.
///
/// Устойчивость к потерям даётся не слоями, а ретрансмиссией и защитой ключевого кадра — см. #23.
constexpr int kTemporalLayers = 1;

/// Идентификатор расширения заголовка RTP, которым передаётся структура кадров.
///
/// Оба конца — наша сборка, поэтому идентификатор фиксирован, а не согласуется. Это осознанное
/// упрощение того же рода, что и с профилем H.264, и с той же оговоркой: как только появятся звонки
/// между разными версиями, идентификатор придётся согласовывать наравне с кодеком.
constexpr int kGenericFrameDescriptorExtensionId = 5;

/// Идентификатор расширения «абсолютное время отправки». СЕЙЧАС НЕ РЕГИСТРИРУЕТСЯ — вместе с ним
/// выключена добивка. Оставлен, потому что вернётся вместе с ней.
///
/// ЗАЧЕМ ОНО, ЕСЛИ ОЦЕНКА КАНАЛА У НАС ПО RFC 8888. Затем, что без него апстрим не выпускает
/// добивку вовсе: `RTPSender::SupportsPadding()` требует `supports_bwe_extension_`, а тот равен
/// `HasBweExtension(карта расширений)` (`rtp_sender.cc:146-151,361`). Это последний уровень
/// механизма #31: не «пейсер не хочет», а «отправитель не считает себя пригодным для добивки».
///
/// ПОЧЕМУ ДОБИВКА ВЫКЛЮЧЕНА ОБРАТНО. Включённая (вместе с kRtxRedundantPayloads) она подняла цель
/// показа экрана с 348 до 375 кбит/с, а четыре прогона из десяти доводила до 770-810. Цена
/// оказалась несоразмерной: при 10% потерь сборка кадров у приёмника обваливалась с 87-90 до 22
/// из 90, и это при НЕТРОНУТЫХ потерях, переспросах и восстановлении из RTX — пакеты доходили все
/// (471 у приёмника против 458-482 в здоровых прогонах), просто переставали складываться в кадры.
///
/// Счёт по сериям, по 10 прогонов каждая:
///   RTX без добивки:            0 обвалов из 20
///   с добивкой, без разрешения проб без медиапакета: 4 обвала из 20
///   с добивкой и разрешением проб:                   9 обвалов из 20
///
/// То есть обвал вносит добивка, а разрешение проб его усиливает. Механизм внутри приёмника пока
/// НЕ НАЙДЕН: известно только, что пакеты доходят, а сборщик кадров перестаёт отдавать кадры.
/// Возвращать добивку — только после того, как этот механизм будет назван, и с ограничением её
/// скорости (max_padding_rate в StreamsConfig у GoogCC). См. #31 и #56.
constexpr int kAbsoluteSendTimeExtensionId = 2;

/// Карта расширений, ОДНА на отправителя и приёмника.
///
/// Расширение, записанное с одним идентификатором и прочитанное с другим, — это молча потерянная
/// структура: приёмник просто не увидит зависимостей и вернётся к линейной цепочке. Поэтому карта
/// одна и строится в одном месте.
const webrtc::RtpHeaderExtensionMap& VideoRtpExtensionMap()
{
    static const webrtc::RtpHeaderExtensionMap* map = [] {
        auto* built = new webrtc::RtpHeaderExtensionMap();
        built->Register<webrtc::RtpGenericFrameDescriptorExtension00>(
            kGenericFrameDescriptorExtensionId);
        // Карта ОДНА на оба конца. Приёмник этим расширением не пользуется, но регистрация
        // здесь безвредна и держит идентификаторы согласованными на случай возврата добивки:
        // расхождение карт — это молча потерянное расширение, ровно то, ради чего карта и заведена
        // в одном месте. Отправитель его сейчас НЕ регистрирует, см. kAbsoluteSendTimeExtensionId.
        built->Register<webrtc::AbsoluteSendTime>(kAbsoluteSendTimeExtensionId);
        return built;
    }();
    return *map;
}

webrtc::VideoCodec BuildSmokeCodec(NativeVideoCodec codec_kind, int width, int height)
{
    webrtc::VideoCodec codec;
    codec.codecType = NativeVideoCodecType(codec_kind);
    codec.width = static_cast<uint16_t>(width);
    codec.height = static_cast<uint16_t>(height);
    codec.startBitrate = 500;
    codec.maxBitrate = 1'500;
    codec.minBitrate = 100;
    codec.maxFramerate = 30;
    codec.active = true;
    // Keep realtime H.264 from collapsing into blocky output during transient ABR events.
    codec.qpMax = 42;
    codec.numberOfSimulcastStreams = 0;
    // ПРОПУСК КАДРОВ. Без него кодировщик физически не может удержать битрейт, и OpenH264 говорит
    // это открытым текстом в каждый лог: "bEnableFrameSkip = 0, bitrate can't be controlled for
    // RC_QUALITY_MODE, RC_BITRATE_MODE and RC_TIMESTAMP_MODE without enabling skip frame".
    //
    // Флаг не выставлялся никогда, и это и есть корень разбирательства 2026-08-17: мы весь день
    // считали, что кодировщик «не слушается» цели ABR, тогда как мы сами отняли у него
    // единственный механизм, которым цель выполняется. Замер: цель 150 кбит/с -> на проводе 331
    // (программный) и 605 (аппаратный). См. docs/decisions/video-quality-ceilings-and-twcc.md.
    codec.SetFrameDropEnabled(true);
    codec.mode = webrtc::VideoCodecMode::kRealtimeVideo;
    codec.expect_encode_from_texture = false;
    if (codec_kind == NativeVideoCodec::Av1) {
        // libaom: один пространственный и один временной слой, как у H.264 ниже.
        codec.SetScalabilityMode(webrtc::ScalabilityMode::kL1T1);
        codec.qpMax = 56;
        return codec;
    }
    *(codec.H264()) = webrtc::VideoEncoder::GetDefaultH264Settings();
    // ОДИН ВРЕМЕННОЙ СЛОЙ — ПОКА. Два включались и были замерены 2026-08-17; цифры в решении
    // docs/decisions/video-quality-ceilings-and-twcc.md.
    //
    // Кратко: структура работает на обоих путях (ровно половина кадров выходит с nal_ref_idc == 0,
    // то есть на них никто не ссылается), но стоит качества при том же битрейте — аппаратному
    // пути 10.7 дБ PSNR, программному 0.7. А выгода слоёв — переживание потерь без ключевого кадра
    // — требует, чтобы ЕЙ ПОЛЬЗОВАЛСЯ ПРИЁМНИК, а он пока не умеет. Платить десять децибел
    // постоянно за преимущество, которого нет, нельзя.
    //
    // Порядок обратный: сначала приёмник учится строить ссылки по временной структуре, потом
    // замеряется выигрыш под инжектированными потерями, и только тогда сравниваются обе величины.
    // Счётчики frames_reference / frames_non_reference оставлены — они и есть проверка, что слои
    // включились, когда до этого дойдёт.
    codec.H264()->SetNumberOfTemporalLayers(static_cast<unsigned char>(kTemporalLayers));
    (void)codec_kind;
    return codec;
}

int ClampInt(int value, int minimum, int maximum)
{
    return std::min(std::max(value, minimum), maximum);
}

std::optional<int> FindJsonInt(std::string_view json, std::string_view property)
{
    const std::string quoted_property = "\"" + std::string(property) + "\"";
    const size_t property_index = json.find(quoted_property);
    if (property_index == std::string_view::npos) {
        return std::nullopt;
    }

    const size_t colon_index = json.find(':', property_index + quoted_property.size());
    if (colon_index == std::string_view::npos) {
        return std::nullopt;
    }

    size_t value_start = colon_index + 1;
    while (value_start < json.size() &&
           std::isspace(static_cast<unsigned char>(json[value_start])) != 0) {
        ++value_start;
    }

    size_t value_end = value_start;
    if (value_end < json.size() && json[value_end] == '-') {
        ++value_end;
    }

    while (value_end < json.size() &&
           std::isdigit(static_cast<unsigned char>(json[value_end])) != 0) {
        ++value_end;
    }

    if (value_end == value_start) {
        return std::nullopt;
    }

    try {
        return std::stoi(std::string(json.substr(value_start, value_end - value_start)));
    } catch (...) {
        return std::nullopt;
    }
}

std::optional<std::string> FindJsonString(std::string_view json, std::string_view property)
{
    const std::string quoted_property = "\"" + std::string(property) + "\"";
    const size_t property_index = json.find(quoted_property);
    if (property_index == std::string_view::npos) {
        return std::nullopt;
    }

    const size_t colon_index = json.find(':', property_index + quoted_property.size());
    if (colon_index == std::string_view::npos) {
        return std::nullopt;
    }

    size_t quote_start = json.find('"', colon_index + 1);
    if (quote_start == std::string_view::npos) {
        return std::nullopt;
    }

    std::string value;
    bool escaped = false;
    for (size_t index = quote_start + 1; index < json.size(); ++index) {
        const char ch = json[index];
        if (escaped) {
            value.push_back(ch);
            escaped = false;
            continue;
        }

        if (ch == '\\') {
            escaped = true;
            continue;
        }

        if (ch == '"') {
            return value;
        }

        value.push_back(ch);
    }

    return std::nullopt;
}

uint16_t ReadLittleEndian16(const uint8_t* source)
{
    return static_cast<uint16_t>(
        static_cast<uint16_t>(source[0]) |
        (static_cast<uint16_t>(source[1]) << 8u));
}

uint16_t ReadBigEndian16(const uint8_t* source)
{
    return static_cast<uint16_t>(
        (static_cast<uint16_t>(source[0]) << 8u) |
        static_cast<uint16_t>(source[1]));
}

uint32_t ReadBigEndian32(const uint8_t* source)
{
    return (static_cast<uint32_t>(source[0]) << 24u) |
           (static_cast<uint32_t>(source[1]) << 16u) |
           (static_cast<uint32_t>(source[2]) << 8u) |
           static_cast<uint32_t>(source[3]);
}

/// ВИРТУАЛЬНЫЕ ЧАСЫ СТЕНДОВ (#59). В продукте всегда nullptr.
///
/// Держатся на процесс, а не на сессию, потому что подменять надо ОКРУЖЕНИЕ, которое сессия
/// берёт при создании, — а создаёт она его сама.
std::mutex g_simulated_time_mutex;
std::unique_ptr<webrtc::GlobalSimulatedTimeController> g_simulated_time;

/// Часы, по которым живёт ВСЁ НАШЕ время.
///
/// ДО #59 ИХ БЫЛО ДВА, И ЭТО ДЕФЕКТ САМ ПО СЕБЕ. Апстрим внутри сессии читает env_.clock(), а наш
/// MonotonicNowMs брал std::chrono::steady_clock напрямую — в одиннадцати местах. В продукте оба
/// источника реальные и монотонные, поэтому расхождение не проявлялось; но это совпадение, а не
/// свойство. Стоит подменить часы у окружения — и половина системы живёт в одном времени, а
/// половина в другом: такт ABR, интервал ключевых кадров, устаревание отчётов о доставке.
///
/// Теперь источник один. Под виртуальными часами он виртуальный для ОБЕИХ половин.
int64_t OmniNowMs()
{
    {
        std::lock_guard<std::mutex> lock(g_simulated_time_mutex);
        if (g_simulated_time) {
            return g_simulated_time->GetClock()->TimeInMilliseconds();
        }
    }
    return std::chrono::duration_cast<std::chrono::milliseconds>(
        std::chrono::steady_clock::now().time_since_epoch()).count();
}

int64_t MonotonicNowMs()
{
    return OmniNowMs();
}

// TimeNowUs УБРАНА ВМЕСТЕ С ЕДИНСТВЕННЫМ ПОТРЕБИТЕЛЕМ (#61, шаг D2b): время съёмки кадра
// для детектора перегрузки теперь ставит сам оркестратор кодирования.


int64_t EffectiveNowMs(int64_t now_ms)
{
    return now_ms > 0 ? now_ms : MonotonicNowMs();
}

std::string_view AbrStateName(uint32_t state)
{
    switch (state) {
    case OMNI_WEBRTC_MEDIA_ABR_STATE_HOLD_CONGESTED_PROBE:
        return "hold:congested_probe";
    case OMNI_WEBRTC_MEDIA_ABR_STATE_HOLD_MODERATE_PROBE:
        return "hold:moderate_probe";
    case OMNI_WEBRTC_MEDIA_ABR_STATE_DOWN_LOCAL_PROCESSING:
        return "down:local_processing";
    case OMNI_WEBRTC_MEDIA_ABR_STATE_DOWN_CONGESTED:
        return "down:congested";
    case OMNI_WEBRTC_MEDIA_ABR_STATE_DOWN_MODERATE:
        return "down:moderate";
    case OMNI_WEBRTC_MEDIA_ABR_STATE_UP_STABLE:
        return "up:stable";
    case OMNI_WEBRTC_MEDIA_ABR_STATE_STABLE:
    default:
        return "stable";
    }
}

#if defined(_WIN32)
std::string WideToUtf8(const wchar_t* value)
{
    if (value == nullptr || value[0] == L'\0') {
        return {};
    }

    const int required = WideCharToMultiByte(CP_UTF8, 0, value, -1, nullptr, 0, nullptr, nullptr);
    if (required <= 1) {
        return {};
    }

    std::string utf8(static_cast<size_t>(required - 1), '\0');
    (void)WideCharToMultiByte(CP_UTF8, 0, value, -1, utf8.data(), required, nullptr, nullptr);
    return utf8;
}

std::string HResultString(HRESULT hr)
{
    std::ostringstream stream;
    stream << "0x" << std::hex << std::setw(8) << std::setfill('0')
           << static_cast<uint32_t>(hr);
    return stream.str();
}

// Audio Phase 2a-step2 (2026-05-21): WASAPI shared-mode capture
// skeleton. Opens the default capture endpoint, negotiates an
// IAudioClient at 48 kHz mono PCM16 shared mode, and runs a worker
// thread that polls IAudioCaptureClient every ~10 ms. Each pulled
// chunk is converted to int16 PCM (in-place when feasible) and
// handed to a user-supplied callback. Resampling happens here when
// the device's mix format doesn't match 48 kHz mono — common on
// hardware that defaults to 44.1 kHz stereo. The callback is
// per-pulled-frame, not per-20ms-opus-frame; the consumer (step 3:
// opus encode) is responsible for accumulating 960 samples before
// calling opus_encode.
//
// Lifecycle invariant: Start() either returns OK with the worker
// alive, or Err with everything torn down. Stop() is idempotent and
// joins the worker before returning.
//
// Headless / no-mic safety: in shared mode IAudioClient::Initialize
// succeeds on machines without a capture device (the OS exposes a
// virtual loopback / null endpoint). The worker simply observes
// zero packet-frames per pull and never invokes the callback. The
// smoke at the bottom of this TU relies on this: Start + 50 ms +
// Stop must complete cleanly even without a microphone.
//
// See docs/decisions/audio-phase-1-adoption.md.
/// ИМЯ УСТРОЙСТВА, КОТОРОЕ МЫ РЕАЛЬНО ОТКРЫЛИ.
///
/// Windows держит ДВА устройства по умолчанию: обычное (`eConsole`) и устройство связи
/// (`eCommunications`). Звонок открывал ВТОРОЕ, а человек переключает первое и слышит тишину;
/// с 2026-10-11 звонок открывает ПЕРВОЕ (второе — только если первого нет).
///
/// Живой замер 2026-08-28: динамики переключены, будильник в комнате слышно, а микрофон звонка
/// отдаёт Opus DTX (23 байта на пакет) — приложение и комната были на разных устройствах, и
/// понять это по диагностике было нельзя. Теперь можно.
inline std::string ReadAudioEndpointName(IMMDevice* device)
{
    if (device == nullptr) {
        return std::string();
    }
    Microsoft::WRL::ComPtr<IPropertyStore> properties;
    if (FAILED(device->OpenPropertyStore(STGM_READ, properties.GetAddressOf())) || !properties) {
        return std::string();
    }
    PROPVARIANT value;
    ::PropVariantInit(&value);
    std::string name;
    if (SUCCEEDED(properties->GetValue(PKEY_Device_FriendlyName, &value)) &&
        value.vt == VT_LPWSTR && value.pwszVal != nullptr) {
        name = WideToUtf8(value.pwszVal);
    }
    ::PropVariantClear(&value);
    return name;
}

/// Имя устройства воспроизведения, открытого звонком.
///
/// Пишет воспроизведение, читает диагностика эхоподавителя — а она живёт на стороне ЗАХВАТА,
/// там же, где APM. Тот же приём, что и с `g_echo_reverse_blocks`: две половины дуплекса
/// разные объекты, и связать их может только общее место.
inline std::mutex& OmniRenderDeviceNameMutex()
{
    static std::mutex mutex;
    return mutex;
}

inline std::string& OmniRenderDeviceNameStorage()
{
    static std::string name;
    return name;
}

inline void OmniPublishRenderDeviceName(std::string name)
{
    std::lock_guard<std::mutex> lock(OmniRenderDeviceNameMutex());
    OmniRenderDeviceNameStorage() = std::move(name);
}

inline std::string OmniRenderDeviceName()
{
    std::lock_guard<std::mutex> lock(OmniRenderDeviceNameMutex());
    return OmniRenderDeviceNameStorage();
}

inline std::wstring Utf8ToWide(std::string_view value)
{
    if (value.empty()) {
        return {};
    }
    const int required = MultiByteToWideChar(
        CP_UTF8, 0, value.data(), static_cast<int>(value.size()), nullptr, 0);
    if (required <= 0) {
        return {};
    }
    std::wstring wide(static_cast<size_t>(required), L'\0');
    (void)MultiByteToWideChar(
        CP_UTF8, 0, value.data(), static_cast<int>(value.size()), wide.data(), required);
    return wide;
}

/// Устройство звонка: выбранное человеком в «Настройки → Звонки», иначе — по умолчанию.
///
/// Выбранное берётся, только если оно есть и активно; пропавшее (гарнитуру выдернули) молча
/// уступает устройству по умолчанию — как с камерой: устаревший выбор не должен оставить звонок
/// без звука. Что открыто на самом деле, видно в диагностике эхоподавителя (`mic=` / `spkdev=`).
///
/// По умолчанию — ОБЫЧНОЕ устройство (eConsole), то, что человек выбирает в параметрах звука
/// Windows, и только без него — устройство связи. Было наоборот, и дважды звонок уходил немым,
/// потому что устройством связи было другое: 2026-08-28 гарнитура, 2026-10-11 «Steam Streaming
/// Microphone» (Opus DTX, 23 байта на пакет, пока владелец говорил в микрофон по умолчанию).
inline HRESULT OpenAudioEndpoint(
    IMMDeviceEnumerator* enumerator,
    EDataFlow flow,
    const std::wstring& preferred_id,
    IMMDevice** out_device)
{
    if (!preferred_id.empty()) {
        Microsoft::WRL::ComPtr<IMMDevice> chosen;
        DWORD state = 0;
        if (SUCCEEDED(enumerator->GetDevice(preferred_id.c_str(), chosen.GetAddressOf())) &&
            SUCCEEDED(chosen->GetState(&state)) && state == DEVICE_STATE_ACTIVE) {
            *out_device = chosen.Detach();
            return S_OK;
        }
    }
    HRESULT hr = enumerator->GetDefaultAudioEndpoint(flow, eConsole, out_device);
    if (FAILED(hr)) {
        hr = enumerator->GetDefaultAudioEndpoint(flow, eCommunications, out_device);
    }
    return hr;
}

class OmniWasapiCapture {
public:
    using FrameCallback = std::function<void(const int16_t* samples, size_t sample_count)>;

    static constexpr uint32_t kSampleRate = 48000;
    static constexpr uint16_t kChannels = 1;
    static constexpr REFERENCE_TIME kBufferDuration100Ns = 200'000;  // 20 ms

    OmniWasapiCapture() = default;

    /// Имя открытого устройства ввода. Пусто = ещё не открывали.
    const std::string& DeviceName() const { return device_name_; }
    ~OmniWasapiCapture() { Stop(); }

    OmniWasapiCapture(const OmniWasapiCapture&) = delete;
    OmniWasapiCapture& operator=(const OmniWasapiCapture&) = delete;

    /// `device_id` — IMMDevice id выбранного микрофона; пусто = по умолчанию (см. OpenAudioEndpoint).
    HRESULT Start(FrameCallback callback, const std::wstring& device_id = {})
    {
        if (running_.load(std::memory_order_acquire)) {
            return S_FALSE;  // already running, idempotent no-op
        }

        callback_ = std::move(callback);
        HRESULT hr = ::CoInitializeEx(nullptr, COINIT_MULTITHREADED);
        co_initialized_ = SUCCEEDED(hr) && hr != S_FALSE;

        Microsoft::WRL::ComPtr<IMMDeviceEnumerator> enumerator;
        hr = ::CoCreateInstance(
            __uuidof(MMDeviceEnumerator),
            nullptr,
            CLSCTX_ALL,
            __uuidof(IMMDeviceEnumerator),
            reinterpret_cast<void**>(enumerator.GetAddressOf()));
        if (FAILED(hr)) { return TeardownOnError(hr); }

        Microsoft::WRL::ComPtr<IMMDevice> device;
        hr = OpenAudioEndpoint(enumerator.Get(), eCapture, device_id, device.GetAddressOf());
        if (FAILED(hr)) { return TeardownOnError(hr); }
        device_name_ = ReadAudioEndpointName(device.Get());

        hr = device->Activate(
            __uuidof(IAudioClient),
            CLSCTX_ALL,
            nullptr,
            reinterpret_cast<void**>(audio_client_.GetAddressOf()));
        if (FAILED(hr)) { return TeardownOnError(hr); }

        WAVEFORMATEX desired{};
        desired.wFormatTag = WAVE_FORMAT_PCM;
        desired.nChannels = kChannels;
        desired.nSamplesPerSec = kSampleRate;
        desired.wBitsPerSample = 16;
        desired.nBlockAlign =
            static_cast<WORD>(desired.nChannels * desired.wBitsPerSample / 8);
        desired.nAvgBytesPerSec = desired.nSamplesPerSec * desired.nBlockAlign;
        desired.cbSize = 0;

        // Initialise with our fixed 48 kHz mono PCM16 format and let WASAPI
        // convert from the device mix format (AUTOCONVERTPCM) — see the matching
        // note in OmniWasapiRender::Start. Avoids the WAVEFORMATEXTENSIBLE-truncation
        // E_INVALIDARG and guarantees the worker reads our mono format.
        WAVEFORMATEX effective = desired;
        hr = audio_client_->Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            kBufferDuration100Ns,
            /*Periodicity=*/0,
            &effective,
            /*AudioSessionGuid=*/nullptr);
        if (FAILED(hr)) { return TeardownOnError(hr); }

        hr = audio_client_->GetService(
            __uuidof(IAudioCaptureClient),
            reinterpret_cast<void**>(capture_client_.GetAddressOf()));
        if (FAILED(hr)) { return TeardownOnError(hr); }

        effective_format_ = effective;
        running_.store(true, std::memory_order_release);
        stop_requested_.store(false, std::memory_order_release);

        hr = audio_client_->Start();
        if (FAILED(hr)) {
            running_.store(false, std::memory_order_release);
            return TeardownOnError(hr);
        }

        worker_ = std::thread(&OmniWasapiCapture::WorkerLoop, this);
        return S_OK;
    }

    void Stop()
    {
        if (!running_.load(std::memory_order_acquire)) { return; }
        stop_requested_.store(true, std::memory_order_release);
        if (worker_.joinable()) { worker_.join(); }
        if (audio_client_) { audio_client_->Stop(); }
        capture_client_.Reset();
        audio_client_.Reset();
        running_.store(false, std::memory_order_release);
        if (co_initialized_) {
            ::CoUninitialize();
            co_initialized_ = false;
        }
    }

private:
    HRESULT TeardownOnError(HRESULT hr)
    {
        capture_client_.Reset();
        audio_client_.Reset();
        if (co_initialized_) {
            ::CoUninitialize();
            co_initialized_ = false;
        }
        return hr;
    }

    void WorkerLoop()
    {
        while (!stop_requested_.load(std::memory_order_acquire)) {
            UINT32 packet_frames = 0;
            HRESULT hr = capture_client_->GetNextPacketSize(&packet_frames);
            if (FAILED(hr) || packet_frames == 0) {
                std::this_thread::sleep_for(std::chrono::milliseconds(5));
                continue;
            }

            BYTE* data = nullptr;
            UINT32 frame_count = 0;
            DWORD flags = 0;
            hr = capture_client_->GetBuffer(&data, &frame_count, &flags, nullptr, nullptr);
            if (FAILED(hr)) {
                std::this_thread::sleep_for(std::chrono::milliseconds(5));
                continue;
            }

            const bool is_silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT) != 0;
            // For the skeleton we only forward the 48 kHz mono PCM16
            // case directly. Other formats (44.1 kHz, stereo, float)
            // are silently dropped here — the resampling pass lands
            // in step 3 alongside the opus encoder integration.
            if (callback_ &&
                effective_format_.nSamplesPerSec == kSampleRate &&
                effective_format_.nChannels == kChannels &&
                effective_format_.wBitsPerSample == 16) {
                if (is_silent) {
                    std::vector<int16_t> silent(frame_count * kChannels, 0);
                    callback_(silent.data(), silent.size());
                } else if (data != nullptr) {
                    const int16_t* samples = reinterpret_cast<const int16_t*>(data);
                    callback_(samples, static_cast<size_t>(frame_count) * kChannels);
                }
            }

            capture_client_->ReleaseBuffer(frame_count);
        }
    }

    Microsoft::WRL::ComPtr<IAudioClient> audio_client_;
    Microsoft::WRL::ComPtr<IAudioCaptureClient> capture_client_;
    WAVEFORMATEX effective_format_{};
    FrameCallback callback_;
    std::thread worker_;
    std::atomic<bool> running_{false};
    std::atomic<bool> stop_requested_{false};
    bool co_initialized_ = false;
    /// Имя открытого устройства — см. ReadAudioEndpointName.
    std::string device_name_;
};

// Audio Phase 2b-step1 (2026-05-23): WASAPI shared-mode RENDER endpoint.
// Mirrors OmniWasapiCapture in shape: own thread polling the device,
// CoInitializeEx ref-counted, headless-safe (no render endpoint → no-op
// worker). PCM contract is the same: 16-bit signed, 48 kHz mono.
//
// The device PULLS (2026-09-27): each time it has room, the worker asks the PCM source for exactly
// that many samples — the source is the playback's NetEq, which owns jitter, reordering, loss and
// the target delay. There is no queue here any more: the one there was only grew (a late burst
// became permanent latency) and was capped by dropping its oldest second. See
// docs/bugs/audio-latency-only-grew.md.
//
// The polling cadence is ~5 ms (matches capture). For phase 2b the
// timing is good enough; if latency proves bad we can switch to
// event-driven mode (SetEventHandle) without changing the API.
//
// See docs/decisions/audio-phase-1-adoption.md.
class OmniWasapiRender {
public:
    static constexpr uint32_t kSampleRate = 48000;
    static constexpr uint16_t kChannels = 1;
    static constexpr REFERENCE_TIME kBufferDuration100Ns = 600'000;  // 60 ms device buffer

    OmniWasapiRender() = default;
    ~OmniWasapiRender() { Stop(); }

    OmniWasapiRender(const OmniWasapiRender&) = delete;
    OmniWasapiRender& operator=(const OmniWasapiRender&) = delete;

    /// `device_id` — IMMDevice id выбранных динамиков; пусто = по умолчанию (см. OpenAudioEndpoint).
    HRESULT Start(const std::wstring& device_id = {})
    {
        if (running_.load(std::memory_order_acquire)) {
            return S_FALSE;
        }

        HRESULT hr = ::CoInitializeEx(nullptr, COINIT_MULTITHREADED);
        co_initialized_ = SUCCEEDED(hr) && hr != S_FALSE;

        Microsoft::WRL::ComPtr<IMMDeviceEnumerator> enumerator;
        hr = ::CoCreateInstance(
            __uuidof(MMDeviceEnumerator),
            nullptr,
            CLSCTX_ALL,
            __uuidof(IMMDeviceEnumerator),
            reinterpret_cast<void**>(enumerator.GetAddressOf()));
        if (FAILED(hr)) { return TeardownOnError(hr); }

        Microsoft::WRL::ComPtr<IMMDevice> device;
        hr = OpenAudioEndpoint(enumerator.Get(), eRender, device_id, device.GetAddressOf());
        if (FAILED(hr)) { return TeardownOnError(hr); }
        device_name_ = ReadAudioEndpointName(device.Get());
        OmniPublishRenderDeviceName(device_name_);

        hr = device->Activate(
            __uuidof(IAudioClient),
            CLSCTX_ALL,
            nullptr,
            reinterpret_cast<void**>(audio_client_.GetAddressOf()));
        if (FAILED(hr)) { return TeardownOnError(hr); }

        WAVEFORMATEX desired{};
        desired.wFormatTag = WAVE_FORMAT_PCM;
        desired.nChannels = kChannels;
        desired.nSamplesPerSec = kSampleRate;
        desired.wBitsPerSample = 16;
        desired.nBlockAlign =
            static_cast<WORD>(desired.nChannels * desired.wBitsPerSample / 8);
        desired.nAvgBytesPerSec = desired.nSamplesPerSec * desired.nBlockAlign;
        desired.cbSize = 0;

        // Initialise with our fixed 48 kHz mono PCM16 format and let WASAPI
        // convert to the device mix format (AUTOCONVERTPCM). Previously this copied
        // the IsFormatSupported "closest" into a plain WAVEFORMATEX, which truncated
        // any WAVEFORMATEXTENSIBLE the driver returned (cbSize=22 with no payload)
        // and made Initialize fail E_INVALIDARG on render endpoints whose shared mix
        // format is extensible (stereo float). Forcing our format + autoconvert is
        // device-agnostic and keeps the worker's mono assumption correct.
        WAVEFORMATEX effective = desired;
        hr = audio_client_->Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            kBufferDuration100Ns,
            /*Periodicity=*/0,
            &effective,
            /*AudioSessionGuid=*/nullptr);
        if (FAILED(hr)) { return TeardownOnError(hr); }

        hr = audio_client_->GetService(
            __uuidof(IAudioRenderClient),
            reinterpret_cast<void**>(render_client_.GetAddressOf()));
        if (FAILED(hr)) { return TeardownOnError(hr); }

        effective_format_ = effective;
        running_.store(true, std::memory_order_release);
        stop_requested_.store(false, std::memory_order_release);

        hr = audio_client_->Start();
        if (FAILED(hr)) {
            running_.store(false, std::memory_order_release);
            return TeardownOnError(hr);
        }

        worker_ = std::thread(&OmniWasapiRender::WorkerLoop, this);
        return S_OK;
    }

    void Stop()
    {
        if (!running_.load(std::memory_order_acquire)) { return; }
        stop_requested_.store(true, std::memory_order_release);
        if (worker_.joinable()) { worker_.join(); }
        if (audio_client_) { audio_client_->Stop(); }
        render_client_.Reset();
        audio_client_.Reset();
        running_.store(false, std::memory_order_release);
        if (co_initialized_) {
            ::CoUninitialize();
            co_initialized_ = false;
        }
    }

    /// Where the device takes its audio: fill `dst` with exactly `sample_count` samples. Called
    /// from the render worker only; set before Start. No source = silence.
    using PcmSource = std::function<void(int16_t* dst, size_t sample_count)>;

    void SetPcmSource(PcmSource source)
    {
        pcm_source_ = std::move(source);
    }

    /// КОЛБЭК «ЭТИ СЭМПЛЫ УХОДЯТ В УСТРОЙСТВО».
    ///
    /// Нужен эхоподавителю: ему требуется опорный сигнал, выровненный по времени с тем, что
    /// РЕАЛЬНО прозвучит. Момент записи в очередь для этого не годится — между ним и динамиком
    /// стоит липсинк-задержка (до 500 мс) и буфер WASAPI (~60 мс).
    ///
    /// Про APM этот класс не знает и знать не должен: он сообщает факт «отдал сэмплы», а что с
    /// ним делать, решает сессия воспроизведения. См. docs/bugs/echo-was-invisible.md.
    using RenderedSamplesObserver = std::function<void(const int16_t*, size_t)>;

    void SetRenderedSamplesObserver(RenderedSamplesObserver observer)
    {
        std::lock_guard<std::mutex> lock(observer_mutex_);
        rendered_observer_ = std::move(observer);
    }


private:
    HRESULT TeardownOnError(HRESULT hr)
    {
        render_client_.Reset();
        audio_client_.Reset();
        if (co_initialized_) {
            ::CoUninitialize();
            co_initialized_ = false;
        }
        return hr;
    }

    void WorkerLoop()
    {
        UINT32 total_frames = 0;
        if (FAILED(audio_client_->GetBufferSize(&total_frames)) || total_frames == 0) {
            return;
        }

        while (!stop_requested_.load(std::memory_order_acquire)) {
            UINT32 padding = 0;
            HRESULT hr = audio_client_->GetCurrentPadding(&padding);
            if (FAILED(hr) || padding >= total_frames) {
                std::this_thread::sleep_for(std::chrono::milliseconds(5));
                continue;
            }

            const UINT32 frames_available = total_frames - padding;
            if (frames_available == 0) {
                std::this_thread::sleep_for(std::chrono::milliseconds(5));
                continue;
            }

            // Skeleton only forwards the 48 kHz mono PCM16 format. If
            // the device divergence forced a different effective
            // format, treat the render path as inactive — production
            // resampler lands later.
            if (effective_format_.nSamplesPerSec != kSampleRate ||
                effective_format_.nChannels != kChannels ||
                effective_format_.wBitsPerSample != 16) {
                std::this_thread::sleep_for(std::chrono::milliseconds(20));
                continue;
            }

            BYTE* data = nullptr;
            hr = render_client_->GetBuffer(frames_available, &data);
            if (FAILED(hr) || data == nullptr) {
                std::this_thread::sleep_for(std::chrono::milliseconds(5));
                continue;
            }

            const size_t samples_needed =
                static_cast<size_t>(frames_available) * kChannels;
            int16_t* dst = reinterpret_cast<int16_t*>(data);
            // Копия того же блока для наблюдателя: читать `dst` после ReleaseBuffer нельзя —
            // буфер уже не наш.
            rendered_scratch_.assign(samples_needed, 0);
            if (pcm_source_) {
                pcm_source_(rendered_scratch_.data(), samples_needed);
            }
            std::memcpy(dst, rendered_scratch_.data(), samples_needed * sizeof(int16_t));

            render_client_->ReleaseBuffer(frames_available, 0);

            // ОПОРНЫЙ СИГНАЛ СНИМАЕТСЯ ЗДЕСЬ, А НЕ ПРИ ДЕКОДИРОВАНИИ.
            //
            // Сюда попадает ровно то, что уйдёт в динамик, — включая тишину добивки. Тишину
            // отдаём НАМЕРЕННО: эхоподавителю нужен непрерывный поток, выровненный по времени, а
            // не только громкие куски. Пропуски в опорном сигнале он читает как рассинхрон.
            //
            // Наблюдателя зовём ПОСЛЕ ReleaseBuffer: обработка блока не должна задерживать устройство.
            NotifyRenderedSamples(samples_needed);
        }
    }

    /// Скопировать отданный устройству блок наблюдателю. Зовётся только из воркера.
    void NotifyRenderedSamples(size_t sample_count)
    {
        RenderedSamplesObserver observer;
        {
            std::lock_guard<std::mutex> lock(observer_mutex_);
            observer = rendered_observer_;
        }
        if (!observer || sample_count == 0) {
            return;
        }
        observer(rendered_scratch_.data(), sample_count);
    }

    Microsoft::WRL::ComPtr<IAudioClient> audio_client_;
    Microsoft::WRL::ComPtr<IAudioRenderClient> render_client_;
    WAVEFORMATEX effective_format_{};
    PcmSource pcm_source_;
    /// Последний блок, отданный устройству. Трогает только воркер.
    std::vector<int16_t> rendered_scratch_;
    std::mutex observer_mutex_;
    RenderedSamplesObserver rendered_observer_;
    std::thread worker_;
    std::atomic<bool> running_{false};
    std::atomic<bool> stop_requested_{false};
    bool co_initialized_ = false;
    /// Имя открытого устройства — см. ReadAudioEndpointName.
    std::string device_name_;
};
#endif

#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
// Audio Phase 2a-step3 (2026-05-21): OmniAudioSender ties the pieces
// together. PCM accumulates into a 960-sample frame buffer (20 ms at
// 48 kHz). When the buffer is full, we call `opus_encode`, build an
// RTP packet (header + opus payload) and hand it to the packet output
// at once (SetPacketOut) — no queue; see OmniPacketSink.
//
// Threading: PushPcm is callable from any thread (typically the
// WASAPI capture worker), serialized internally by a mutex; the
// encoder + accumulator live behind it, and the output is called
// under it.
//
// SSRC: deliberately distinct from the video sender SSRC. The first
// 4 bytes spell "OMNA" (Omni Audio) for diagnostic readability.
// Wire format: standard RTP (RFC 3550 minimal header) + opus payload
// (RFC 7587). RTP timestamp clock = 48 kHz (per RFC 7587 §4.1, even
// for narrowband internal sample rates).
//
// See docs/decisions/audio-phase-1-adoption.md.
// ИСХОДЯЩИЙ ПАКЕТ УХОДИТ В ЯДРО СРАЗУ, А НЕ В ОЧЕРЕДЬ (2026-09-26).
//
// До этого у каждой сессии была своя исходящая очередь, и оболочка выгребала её таймером: на
// Windows это ~15.6 мс. Пейсер libwebrtc раздавал пакеты ровно, а таймер собирал их в пачки и
// отправлял залпом; время отправки для регулятора при этом снималось ДО ожидания такта. Рядом с
// настоящим пейсером жили своя очередь, своё вытеснение кадров и свои счётчики глубины.
// См. docs/bugs/media-rode-the-shells-timer.md.
//
// Теперь как у апстрима: транспорт отдаёт пакет получателю в тот же момент, когда его выпустил
// пейсер. Получатель — функция ядра (omni_realtime_media_packet_sink) со своим контекстом.
//
// Замок держится на время вызова НАМЕРЕННО: Set(nullptr, nullptr) возвращается только когда ни
// одного вызова в полёте нет, и после этого контекст можно освобождать.
class OmniPacketSink {
public:
    void Set(OmniWebRtcMediaPacketSink sink, void* context)
    {
        std::lock_guard<std::mutex> lock(mutex_);
        sink_ = sink;
        context_ = context;
    }

    bool Send(const uint8_t* data, size_t size)
    {
        if (data == nullptr || size == 0) {
            return false;
        }
        std::lock_guard<std::mutex> lock(mutex_);
        if (sink_ == nullptr) {
            unrouted_.fetch_add(1, std::memory_order_relaxed);
            return false;
        }
        sink_(context_, data, size);
        handed_.fetch_add(1, std::memory_order_relaxed);
        return true;
    }

    // Отдано получателю / выпущено, когда получателя не было (он ставится при старте звонка;
    // ненулевое второе число посреди звонка — это провод, который забыли подключить).
    uint64_t handed() const { return handed_.load(std::memory_order_relaxed); }
    uint64_t unrouted() const { return unrouted_.load(std::memory_order_relaxed); }

private:
    std::mutex mutex_;
    OmniWebRtcMediaPacketSink sink_ = nullptr;
    void* context_ = nullptr;
    std::atomic<uint64_t> handed_{0};
    std::atomic<uint64_t> unrouted_{0};
};

class OmniAudioSender {
public:
    static constexpr uint32_t kSampleRate = 48000;
    static constexpr uint16_t kChannels = 1;
    static constexpr int kFrameSamples = 960;   // 20 ms at 48 kHz
    static constexpr int kMaxPacketBytes = 1500;
    static constexpr uint32_t kAudioSsrc = 0x4F4D4E41u;  // "OMNA"
    static constexpr uint8_t kDefaultPayloadType = 111;  // dynamic PT, matches WebRTC convention

    static std::unique_ptr<OmniAudioSender> Create(int target_bitrate_bps, std::string* error)
    {
        int opus_error = 0;
        OpusEncoder* encoder = opus_encoder_create(
            static_cast<opus_int32>(kSampleRate),
            kChannels,
            OPUS_APPLICATION_VOIP,
            &opus_error);
        if (opus_error != OPUS_OK || encoder == nullptr) {
            if (error) {
                *error = std::string("opus_encoder_create failed: ") + opus_strerror(opus_error);
            }
            return nullptr;
        }
        opus_encoder_ctl(encoder, OPUS_SET_BITRATE(target_bitrate_bps));
        opus_encoder_ctl(encoder, OPUS_SET_VBR(1));
        opus_encoder_ctl(encoder, OPUS_SET_DTX(0));  // disabled for phase 2a (predictable wire flow)
        // Audio quality (2026-06-05): in-band forward error correction + an expected
        // packet-loss hint so the encoder embeds recoverable redundancy of the previous frame
        // (NetEq's Opus decoder pulls it back on loss — see OmniNetEqAudioReceiver), and the
        // VOIP signal type + max complexity for the best speech quality the 48 kHz wideband
        // mode can give. The loss hint is a static internet-grade default; a future pass can
        // make it adaptive from RTCP-reported loss.
        opus_encoder_ctl(encoder, OPUS_SET_INBAND_FEC(1));
        opus_encoder_ctl(encoder, OPUS_SET_PACKET_LOSS_PERC(10));
        opus_encoder_ctl(encoder, OPUS_SET_SIGNAL(OPUS_SIGNAL_VOICE));
        opus_encoder_ctl(encoder, OPUS_SET_COMPLEXITY(10));
        auto sender = std::unique_ptr<OmniAudioSender>(new OmniAudioSender());
        sender->encoder_ = encoder;
        sender->target_bitrate_bps_ = target_bitrate_bps;
        return sender;
    }

    ~OmniAudioSender()
    {
        if (encoder_) {
            opus_encoder_destroy(encoder_);
            encoder_ = nullptr;
        }
    }

    OmniAudioSender(const OmniAudioSender&) = delete;
    OmniAudioSender& operator=(const OmniAudioSender&) = delete;

    // PCM contract: 16-bit signed, 48 kHz, mono, interleaved. Caller is
    // responsible for resampling if the device produced a different
    // format (the WASAPI capture skeleton in step 2 drops divergent
    // formats; step 3 keeps that policy until a real resampler lands).
    void PushPcm(const int16_t* samples, size_t sample_count)
    {
        if (samples == nullptr || sample_count == 0) {
            return;
        }
        std::lock_guard<std::mutex> lock(mutex_);
        accumulator_.insert(accumulator_.end(), samples, samples + sample_count);
        while (accumulator_.size() >= static_cast<size_t>(kFrameSamples)) {
            EncodeAndEmitOneFrameUnsafe();
        }
    }

    // Where each encoded RTP packet goes. Set before the first PushPcm.
    using PacketOut = std::function<void(const uint8_t* data, size_t size)>;
    void SetPacketOut(PacketOut out)
    {
        std::lock_guard<std::mutex> lock(mutex_);
        packet_out_ = std::move(out);
    }


private:
    OmniAudioSender() = default;

    void EncodeAndEmitOneFrameUnsafe()
    {
        std::array<unsigned char, kMaxPacketBytes - 12> opus_buf{};  // -12 for RTP header
        const int encoded = opus_encode(
            encoder_,
            accumulator_.data(),
            kFrameSamples,
            opus_buf.data(),
            static_cast<opus_int32>(opus_buf.size()));
        // Always advance the accumulator even if the encoder dropped
        // the frame (e.g. < 0 return) — the data is already consumed
        // semantically; pretending otherwise would back-pressure
        // forever on a transient codec error.
        accumulator_.erase(
            accumulator_.begin(),
            accumulator_.begin() + kFrameSamples);
        if (encoded <= 0) {
            return;
        }

        std::vector<uint8_t> packet;
        packet.reserve(12 + static_cast<size_t>(encoded));
        // RFC 3550 minimal RTP header (12 bytes, no CSRC, no extension):
        //   byte0 = V(2)<<6 | P(0)<<5 | X(0)<<4 | CC(0) = 0x80
        //   byte1 = M(0)<<7 | PT
        //   bytes 2-3 = sequence (big-endian)
        //   bytes 4-7 = timestamp (big-endian)
        //   bytes 8-11 = SSRC (big-endian)
        packet.push_back(0x80);
        packet.push_back(payload_type_ & 0x7F);
        packet.push_back(static_cast<uint8_t>((sequence_ >> 8) & 0xFF));
        packet.push_back(static_cast<uint8_t>(sequence_ & 0xFF));
        packet.push_back(static_cast<uint8_t>((rtp_timestamp_ >> 24) & 0xFF));
        packet.push_back(static_cast<uint8_t>((rtp_timestamp_ >> 16) & 0xFF));
        packet.push_back(static_cast<uint8_t>((rtp_timestamp_ >> 8) & 0xFF));
        packet.push_back(static_cast<uint8_t>(rtp_timestamp_ & 0xFF));
        packet.push_back(static_cast<uint8_t>((kAudioSsrc >> 24) & 0xFF));
        packet.push_back(static_cast<uint8_t>((kAudioSsrc >> 16) & 0xFF));
        packet.push_back(static_cast<uint8_t>((kAudioSsrc >> 8) & 0xFF));
        packet.push_back(static_cast<uint8_t>(kAudioSsrc & 0xFF));
        packet.insert(packet.end(), opus_buf.begin(), opus_buf.begin() + encoded);
        if (packet_out_) {
            packet_out_(packet.data(), packet.size());
        }

        sequence_ = static_cast<uint16_t>(sequence_ + 1);
        // 48 kHz clock, opus frame = 960 samples → timestamp += 960
        rtp_timestamp_ += static_cast<uint32_t>(kFrameSamples);
    }

    OpusEncoder* encoder_ = nullptr;
    int target_bitrate_bps_ = 32000;
    uint8_t payload_type_ = kDefaultPayloadType;
    uint16_t sequence_ = 0;
    uint32_t rtp_timestamp_ = 0;
    std::vector<opus_int16> accumulator_;
    PacketOut packet_out_;
    mutable std::mutex mutex_;
};

#endif

/// ВХОД И ВЫХОД АППАРАТНОГО КОДИРОВЩИКА, СЧИТАННЫЕ ВНУТРИ НЕГО.
///
/// Живой звонок 2026-08-27: кадры входят в оркестратор, наружу не выходит ничего, и ВСЕ потери
/// помечены `kEncoderQueue` — то есть очередь кодирования занята предыдущим кадром
/// (docs/bugs/encoder-goes-silent-mid-call.md). Виноватых двое: либо MFT перестал отдавать выход,
/// либо застревает наш путь после него. Различает их одно — сколько раз кодировщик позван,
/// сколько раз вернулся и сколько занял самый долгий вызов.
///
/// Держится в shared_ptr НАМЕРЕННО: кодировщиком владеет VideoStreamEncoder и пересоздаёт его при
/// каждой перенастройке. Сырой указатель отсюда уже приводил к ACCESS_VIOLATION в диагностике
/// (#61, шаг D2b), и повторять это ради счётчиков нельзя.
///
/// Объявлена ВНЕ windows-only блока: её держат фабрика и сессия, которые собираются всегда.
struct OmniEncoderIoStats
{
    // ИСХОДЫ РАЗБИТЫ ТАК, ЧТОБЫ СУММА СХОДИЛАСЬ: ok + empty + errors + (в полёте) == calls.
    //
    // Первая версия считала «пусто» и «ошибка» двумя ветками без третьей, и в замере они вышли
    // РАВНЫМИ во всех пяти отсчётах (4/4, 9/9, 15/15, 21/21, 27/27). Ветки взаимоисключающие,
    // так что равенство пять раз подряд означало не «так совпало», а «читается не то, что
    // думаешь». Проверять это должен сам набор чисел, а не глаз: если сумма не сходится с
    // calls, врёт прибор, и дальше по нему выводы делать нельзя.
    std::atomic<uint64_t> encode_calls{0};
    std::atomic<uint64_t> encode_returns{0};
    std::atomic<uint64_t> encode_ok{0};
    std::atomic<uint64_t> encode_empty{0};
    std::atomic<uint64_t> encode_errors{0};
    /// Сколько раз мы попросили апстрим уйти на программный кодировщик. Без этого числа откат
    /// посреди звонка невидим: он молчаливый по устройству.
    std::atomic<uint64_t> fallback_requests{0};
    std::atomic<int64_t> max_encode_ms{0};
    std::atomic<int64_t> last_encode_ms{0};
    // Сумма длительностей — чтобы СРЕДНЕЕ считалось делением суммы на число вызовов, а не
    // делением числа вызовов на стенное время. Вторая арифметика уже дала мне ложный вывод
    // «кодировщик тратит 300 мс на кадр», хотя это оркестратор звал его редко.
    std::atomic<int64_t> total_encode_ms{0};
    /// Последняя ошибка САМОГО кодировщика, его словами. Без неё «err растёт» не говорит ничего.
    std::mutex last_error_mutex;
    std::string last_error;
};

#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
// КАКОЙ АППАРАТНЫЙ КОДИРОВЩИК ПРОШЁЛ ПРОВЕРКУ — ТОТ И БЕРЁМ В ЗВОНКЕ (2026-10-14).
//
// На ноутбуке два HEVC-MFT: Intel QSV и NVIDIA. Проверка при старте (`SmokeHardwareEncoder`, кадр
// 320x240) проходила на NVIDIA — Intel такой маленький кадр не настраивает и пропускается. В звонке
// (1280x720) Intel настраивался ПЕРВЫМ по порядку Windows и вставал: METransformNeedInput раз в
// ~350 мс, 407 кадров на входе, 0 на выходе, звонок без картинки. Проверка подтверждала одно
// устройство, а работало другое. Теперь процесс помнит, какой MFT прошёл проверку для кодека, и
// пробует его первым; остальные — следом, как раньше. См. docs/bugs/a-stuck-encoder-was-never-struck.md.
std::mutex g_verified_hardware_encoders_mutex;
std::map<int, std::string> g_verified_hardware_encoders;

void RememberVerifiedHardwareEncoder(NativeVideoCodec codec, const std::string& name)
{
    std::lock_guard<std::mutex> lock(g_verified_hardware_encoders_mutex);
    g_verified_hardware_encoders[static_cast<int>(codec)] = name;
}

std::string VerifiedHardwareEncoder(NativeVideoCodec codec)
{
    std::lock_guard<std::mutex> lock(g_verified_hardware_encoders_mutex);
    const auto found = g_verified_hardware_encoders.find(static_cast<int>(codec));
    return found == g_verified_hardware_encoders.end() ? std::string() : found->second;
}

class MediaFoundationH264Encoder {
public:
    // required_vendor_id (PCI vendor, e.g. 0x10DE NVIDIA / 0x8086 Intel / 0x1002 AMD) restricts the
    // chosen HW encoder MFT to that vendor; 0 = any (production default, picks the first that
    // configures). The capability probe uses a vendor filter to test each adapter's encoder honestly.
    // `codec`: the same MFT contract carries H.264, HEVC and AV1 (NVENC/QSV/AMF expose all three
    // as hardware encoder MFTs); only the output subtype, the profile and the bitstream bookkeeping
    // differ (docs/decisions/per-pair-video-codec-selection.md §9, phase 3).
    static std::unique_ptr<MediaFoundationH264Encoder> Create(
        int width,
        int height,
        int fps,
        int bitrate_bps,
        std::string& error,
        UINT required_vendor_id = 0,
        NativeVideoCodec codec = NativeVideoCodec::H264)
    {
        std::unique_ptr<MediaFoundationH264Encoder> encoder(new MediaFoundationH264Encoder());
        encoder->codec_ = codec;
        if (!encoder->Initialize(width, height, fps, bitrate_bps, error, required_vendor_id)) {
            return nullptr;
        }

        return encoder;
    }

    ~MediaFoundationH264Encoder()
    {
        event_generator_.Reset();
        if (transform_) {
            (void)transform_->ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            (void)transform_->ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            transform_.Reset();
        }

        if (mf_started_) {
            (void)MFShutdown();
        }

        // COM is per thread: only the thread that initialised it may uninitialise it. The codec is
        // routinely destroyed elsewhere (a decoder built on the first frame is replaced from the
        // UI thread's attach), and an uninitialise there takes COM away from THAT thread.
        if (co_initialized_ && co_init_thread_ == ::GetCurrentThreadId()) {
            CoUninitialize();
        }
    }

    OmniWebRtcMediaStatus EncodeNv12(
        const uint8_t* y_plane,
        int32_t y_stride,
        const uint8_t* uv_plane,
        int32_t uv_stride,
        int64_t timestamp_us,
        bool force_keyframe,
        uint32_t rtp_timestamp,
        EncodedFrame& encoded)
    {
        if (!transform_ || y_plane == nullptr || uv_plane == nullptr) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }

        const size_t nv12_size = Nv12FrameSize();
        Microsoft::WRL::ComPtr<IMFMediaBuffer> input_buffer;
        HRESULT hr = MFCreateMemoryBuffer(static_cast<DWORD>(nv12_size), &input_buffer);
        if (FAILED(hr)) {
            last_error_ = "MFCreateMemoryBuffer(input) failed: " + HResultString(hr);
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }

        BYTE* target = nullptr;
        DWORD max_len = 0;
        DWORD current_len = 0;
        hr = input_buffer->Lock(&target, &max_len, &current_len);
        if (FAILED(hr) || target == nullptr || max_len < nv12_size) {
            last_error_ = "input buffer Lock failed: " + HResultString(hr);
            // If Lock() reported success but max_len was too small, the buffer
            // is *still locked* — we must Unlock before returning to avoid
            // leaking the IMFMediaBuffer's locked state (would eventually
            // hang the MF transform). FAILED(hr) means Lock did not succeed,
            // in which case Unlock is a no-op but harmless.
            // See docs/decisions/native-media-locking.md.
            if (SUCCEEDED(hr)) {
                (void)input_buffer->Unlock();
            }
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }

        CopyNv12Planes(target, y_plane, y_stride, uv_plane, uv_stride);
        (void)input_buffer->Unlock();
        hr = input_buffer->SetCurrentLength(static_cast<DWORD>(nv12_size));
        if (FAILED(hr)) {
            last_error_ = "SetCurrentLength(input) failed: " + HResultString(hr);
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }

        Microsoft::WRL::ComPtr<IMFSample> input_sample;
        hr = MFCreateSample(&input_sample);
        if (FAILED(hr)) {
            last_error_ = "MFCreateSample(input) failed: " + HResultString(hr);
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }

        hr = input_sample->AddBuffer(input_buffer.Get());
        if (FAILED(hr)) {
            last_error_ = "AddBuffer(input) failed: " + HResultString(hr);
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }

        const LONGLONG sample_time = timestamp_us > 0
            ? static_cast<LONGLONG>(timestamp_us * 10)
            : static_cast<LONGLONG>(frames_submitted_) * FrameDuration100Ns();
        (void)input_sample->SetSampleTime(sample_time);
        (void)input_sample->SetSampleDuration(FrameDuration100Ns());

        if (force_keyframe || frames_submitted_ == 0 || force_next_keyframe_) {
            SetCodecUInt32(CODECAPI_AVEncVideoForceKeyFrame, 1);
            force_next_keyframe_ = false;
        }

        // ВХОД ПОДАЁТСЯ ТОЛЬКО ПОСЛЕ METransformNeedInput — ЭТО КОНТРАКТ, А НЕ ОПТИМИЗАЦИЯ.
        //
        // Асинхронный MFT сам говорит, когда готов принять кадр. Подача без спроса отвечает
        // `MF_E_NOTACCEPTING`, и раньше мы это «чинили» на месте: выбирали один выход, ВЫБРАСЫВАЛИ
        // его и пробовали ещё раз. Пока событий не теряли — иногда срабатывало; после того как
        // ожидание выхода начало съедать METransformNeedInput, не срабатывало уже никогда.
        //
        // Не дождались — это НЕ ошибка. Кодировщик занят, кадр пропускаем: у живого видео
        // пропущенный кадр дешевле, чем сорванная сессия. Выход при этом всё равно выбираем.
        if (async_ && !TakeAsyncEvent(METransformNeedInput, std::chrono::milliseconds(100))) {
            DrainAvailableOutputs(rtp_timestamp);
            if (!pending_outputs_.empty()) {
                encoded = std::move(pending_outputs_.front());
                pending_outputs_.pop_front();
                ++frames_encoded_;
            }
            return OMNI_WEBRTC_MEDIA_STATUS_OK;
        }

        hr = transform_->ProcessInput(input_stream_id_, input_sample.Get(), 0);
        if (hr == MF_E_NOTACCEPTING) {
            // Осталось как страховка: если MFT всё же не принял, выбираем ВСЁ, что он держит, и
            // пробуем ещё раз. Выбранные кадры больше не выбрасываются — они уходят в очередь и
            // доедут до провода.
            DrainAvailableOutputs(rtp_timestamp);
            hr = transform_->ProcessInput(input_stream_id_, input_sample.Get(), 0);
        }

        if (FAILED(hr)) {
            last_error_ = name_ + " ProcessInput failed: " + HResultString(hr) + " (" + selection_note_ + ")";
            ++encode_failures_;
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }

        ++frames_submitted_;
        DrainAvailableOutputs(rtp_timestamp);
        if (!pending_outputs_.empty()) {
            encoded = std::move(pending_outputs_.front());
            pending_outputs_.pop_front();
            ++frames_encoded_;
        }

        return OMNI_WEBRTC_MEDIA_STATUS_OK;
    }

    OmniWebRtcMediaStatus SetRates(int bitrate_bps, int fps)
    {
        bitrate_bps_ = std::max(1, bitrate_bps);
        fps_ = std::max(1, fps);

        // Спрашиваем у самого MFT, принимает ли он смену битрейта на живом потоке. Без этого
        // вопроса «SetValue вернул S_OK» читалось как «применено», хотя замер показывал обратное:
        // просим 75 кбит/с, на проводе 591. IsModifiable отвечает на это прямо, и ответ решает, что
        // чинить дальше — пересборку выходного типа или подачу кадров.
        mean_bitrate_modifiable_ = IsCodecPropertyModifiable(CODECAPI_AVEncCommonMeanBitRate) ? 1 : 0;

        bool ok = SetCodecUInt32(CODECAPI_AVEncCommonMeanBitRate, static_cast<uint32_t>(bitrate_bps_));
        ok = SetCodecUInt32(CODECAPI_AVEncCommonMaxBitRate, static_cast<uint32_t>(bitrate_bps_)) && ok;
        if (!ok) {
            ++rate_update_failures_;
        }

        // Битрейт МЕНЯЕТСЯ ВМЕСТЕ С РАЗРЕШЕНИЕМ, а не отдельно от него. MFT на IsModifiable
        // отвечает, что на живом потоке битрейт не меняется, и здесь стояла моя пересборка типов
        // ради одного лишь битрейта. Она работала (605 -> 330 кбит/с), но платила ОБЯЗАТЕЛЬНЫМ
        // ключевым кадром за каждую пересборку — самым большим всплеском, какой мы умеем породить,
        // ровно в тот момент, когда канал хуже всего.
        //
        // Так не делает никто, и не нужно: у апстрима битрейт и разрешение ходят парой (таблица
        // EncoderInfoSettings), а разрешение и так перенастраивает MFT установкой типов. То есть
        // битрейт доезжает до MFT там же, где ему и место, — в ReconfigureResolution, бесплатно.

        return OMNI_WEBRTC_MEDIA_STATUS_OK;
    }

    void RequestKeyframe()
    {
        force_next_keyframe_ = true;
    }

    const std::string& Name() const
    {
        return name_;
    }

    const std::string& LastError() const
    {
        return last_error_;
    }

    uint64_t EncodeFailures() const
    {
        return encode_failures_;
    }

    uint64_t RateUpdateFailures() const
    {
        return rate_update_failures_;
    }

    /// -1 = ещё не спрашивали, 0 = MFT не разрешает менять битрейт на живом потоке, 1 = разрешает.
    int MeanBitrateModifiable() const
    {
        return mean_bitrate_modifiable_;
    }

    bool Async() const
    {
        return async_;
    }

    uint64_t ParameterSetPrepends() const
    {
        return parameter_set_prepends_;
    }

    uint64_t IdrFrames() const
    {
        return idr_frames_;
    }

    uint64_t SpsFrames() const
    {
        return sps_frames_;
    }

    uint64_t PpsFrames() const
    {
        return pps_frames_;
    }

private:
    static constexpr DWORD kStreamId = 0;

    bool Initialize(
        int width,
        int height,
        int fps,
        int bitrate_bps,
        std::string& error,
        UINT required_vendor_id = 0)
    {
        width_ = width;
        height_ = height;
        fps_ = std::max(1, fps);
        bitrate_bps_ = std::max(1, bitrate_bps);

        const HRESULT co_hr = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
        if (SUCCEEDED(co_hr)) {
            co_initialized_ = true;
            co_init_thread_ = ::GetCurrentThreadId();
        } else if (co_hr != RPC_E_CHANGED_MODE) {
            error = "CoInitializeEx failed: " + HResultString(co_hr);
            return false;
        }

        const HRESULT startup_hr = MFStartup(MF_VERSION, MFSTARTUP_LITE);
        if (FAILED(startup_hr)) {
            error = "MFStartup failed: " + HResultString(startup_hr);
            return false;
        }
        mf_started_ = true;

        IMFActivate** activates = nullptr;
        UINT32 activate_count = 0;
        MFT_REGISTER_TYPE_INFO input_type;
        input_type.guidMajorType = MFMediaType_Video;
        input_type.guidSubtype = MFVideoFormat_NV12;
        MFT_REGISTER_TYPE_INFO output_type;
        output_type.guidMajorType = MFMediaType_Video;
        output_type.guidSubtype = OutputSubtype();

        const HRESULT enum_hr = MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            &input_type,
            &output_type,
            &activates,
            &activate_count);
        if (FAILED(enum_hr) || activate_count == 0 || activates == nullptr) {
            error = "Media Foundation hardware " + std::string(NativeVideoCodecName(codec_)) +
                " encoder was not found: " + HResultString(enum_hr);
            if (activates != nullptr) {
                CoTaskMemFree(activates);
            }
            return false;
        }

        // ИСПРАВЛЕНО 2026-08-15. Прежняя записка (task #85) утверждала: «QSV конфигурируется, но
        // падает на ProcessOutput (0x8000ffff) с этой NVENC-настроенной media-type; QSV нуждается в
        // собственной настройке (вероятно, D3D11-текстуры) — отложено; кодирование остаётся на
        // NVENC». **Вывод был неверен.** Дело не в media-type и не в вендоре: 0x8000ffff давали два
        // нарушения контракта MFT в `TryProcessOutput` — выброшенный результат ожидания
        // `METransformHaveOutput` и `MF_E_TRANSFORM_STREAM_CHANGE`, считавшийся фатальным. После их
        // починки QSV кодирует С ТОЙ ЖЕ САМОЙ настройкой: проба даёт
        // «smoke passed via Intel® Quick Sync Video H.264 Encoder MFT».
        //
        // Верным в той записке было одно: перебор устройств НЕ страхует от отказа на исполнении —
        // он проверяет только конфигурацию. Поэтому чинить надо было контракт, а не порядок
        // перебора. См. docs/bugs/hardware-h264-never-started.md.
        //
        // Цикл ниже перебирает все найденные аппаратные MFT и берёт первый, который настроился;
        // теперь на этой машине работают оба (NVENC и QSV). DECODE + render уже переведены на iGPU
        // выбором адаптера в рендерере (d3d11_video_renderer.h).
        std::string last_error;
        std::vector<UINT32> order(activate_count);
        for (UINT32 index = 0; index < activate_count; ++index) {
            order[index] = index;
        }
        const std::string verified = VerifiedHardwareEncoder(codec_);
        // ВЕНДОР «АВТО» ВЫБРАН ПО H.264, А НЕ ПО ЭТОМУ КОДЕКУ.
        //
        // Оболочка в режиме «Авто» отдаёт кодирование встроенной графике ради энергии
        // (CallVideoDeviceSelection: iGPU, если она КОДИРУЕТ H.264). Фильтр вендора приходил сюда и
        // для HEVC/AV1: перебор сужался до Intel, NVIDIA не рассматривался вовсе, и звонок брал
        // Intel HEVC MFT, который у нас встаёт (стенд, 2026-10-14: «tried Intel ... (of 3)»).
        // Фильтр вендора для кодека, отличного от H.264, действует, только если у этого вендора
        // и есть проверенный кодировщик этого кодека; иначе — проверенный.
        if (required_vendor_id != 0 && codec_ != NativeVideoCodec::H264 && !verified.empty() &&
            VendorIdFromFriendlyName(verified) != required_vendor_id) {
            required_vendor_id = 0;
        }
        if (!verified.empty()) {
            std::stable_partition(order.begin(), order.end(), [&](UINT32 index) {
                return activates[index] != nullptr && ReadFriendlyName(activates[index]) == verified;
            });
        }
        std::string tried;
        for (const UINT32 index : order) {
            Microsoft::WRL::ComPtr<IMFActivate> activate;
            activate.Attach(activates[index]);
            activates[index] = nullptr;
            if (!activate) {
                continue;
            }

            name_ = ReadFriendlyName(activate.Get());
            if (required_vendor_id != 0 && VendorIdFromFriendlyName(name_) != required_vendor_id) {
                // Capability probe is asking specifically about another vendor's adapter.
                continue;
            }
            tried += (tried.empty() ? "" : " | ") + name_;
            Microsoft::WRL::ComPtr<IMFTransform> candidate;
            const HRESULT activate_hr = activate->ActivateObject(IID_PPV_ARGS(&candidate));
            if (FAILED(activate_hr) || !candidate) {
                last_error = FriendlyNameForDiagnostics() +
                    " ActivateObject failed: " + HResultString(activate_hr);
                continue;
            }

            transform_ = candidate;
            if (ConfigureTransform(last_error)) {
                // Which MFT this is and who was passed over, for every later error: «the call took
                // the wrong encoder» is otherwise invisible (stand, 2026-10-14).
                selection_note_ = "tried " + tried + " (of " + std::to_string(activate_count) + ")" +
                    (verified.empty() ? "" : ", verified " + verified) +
                    (last_error.empty() ? "" : ", passed over: " + last_error);
                CoTaskMemFree(activates);
                return true;
            }

            tried += " failed";
            transform_.Reset();
        }

        CoTaskMemFree(activates);
        error = last_error.empty()
            ? "Media Foundation hardware H.264 encoder activation failed"
            : last_error;
        return false;
    }

    bool ConfigureTransform(std::string& error)
    {
        // Профили от лучшего к худшему: пробуем High, отступаем до Baseline. Отказ конкретного MFT
        // виден только на SetOutputType, поэтому это цикл, а не предварительный опрос.
        // ПРОФИЛЬ: High, с откатом до Baseline.
        //
        // Baseline — профиль без CABAC и без 8x8-преобразования, то есть заведомо худшее сжатие при
        // том же битрейте. Замер стендом picture-quality (PSNR при фиксированных 800 кбит/с на
        // 1280x720): **52.0 дБ против 40.1**, ценой 8% пакетов на кадр. Двенадцать децибел.
        //
        // История этого решения стоит того, чтобы её тут оставить. Сначала High был отвергнут:
        // включение роняло sender-restart с 58-61 декодированного кадра до 4, и это читалось как
        // «приёмник не тянет профиль». Вывод был неверен, потому что делался по ОДИНОЧНЫМ прогонам
        // теста, разброс которого сам же и был задокументирован. Чередование показало, что при
        // Baseline тот же тест даёт 5 / 59 / 9 / 5 — то есть High ничего не ломал, он лишь делал
        // кадры крупнее и потому стабильно попадал в потолок, из которого Baseline иногда
        // выскакивал. Потолком оказалось переполнение буфера показа, отвергавшее 118 кадров из 150
        // МОЛЧА. После того как отказ стал считаться, а тест — подавать кадры в реальном темпе,
        // приёмник собирает и декодирует 150 из 150 на High, четыре прогона подряд.
        //
        // Отказ конкретного MFT виден только на SetOutputType, поэтому это цикл, а не опрос.
        //
        // ЧЕГО ЗДЕСЬ ВСЁ ЕЩЁ НЕТ: согласования profile-level-id. Пока оба конца — одна и та же
        // сборка, выбор кодировщика безопасен; для звонка между разными версиями нужен обмен тем,
        // что сторона умеет ПРИНИМАТЬ. Это отдельная задача, а не то, что закрывается здесь.
        static constexpr UINT32 kH264Profiles[] = {
            eAVEncH264VProfile_High, eAVEncH264VProfile_Main, eAVEncH264VProfile_Base};
        // HEVC и AV1: основной 8-битный 4:2:0 — единственный, который декодирует всё железо.
        static constexpr UINT32 kH265Profiles[] = {eAVEncH265VProfile_Main_420_8};
        static constexpr UINT32 kAv1Profiles[] = {eAVEncAV1VProfile_Main_420_8};
        const UINT32* profiles = kH264Profiles;
        size_t profile_count = std::size(kH264Profiles);
        if (codec_ == NativeVideoCodec::H265) {
            profiles = kH265Profiles;
            profile_count = std::size(kH265Profiles);
        } else if (codec_ == NativeVideoCodec::Av1) {
            profiles = kAv1Profiles;
            profile_count = std::size(kAv1Profiles);
        }
        for (size_t i = 0; i < profile_count; ++i) {
            requested_profile_ = profiles[i];
            std::string attempt_error;
            if (ConfigureTransformWithCurrentProfile(attempt_error)) {
                return true;
            }
            if (i + 1 == profile_count) {
                error = attempt_error;
                return false;
            }
        }
        return false;
    }

    bool ConfigureTransformWithCurrentProfile(std::string& error)
    {
        if (!transform_) {
            error = "Media Foundation H.264 transform is null";
            return false;
        }

        UnlockAsyncTransform();
        ConfigureCodecPropertiesBeforeTypeSet();

        Microsoft::WRL::ComPtr<IMFMediaType> output_type;
        HRESULT hr = CreateH264OutputType(&output_type);
        if (FAILED(hr)) {
            error = FriendlyNameForDiagnostics() + " output media type creation failed: " + HResultString(hr);
            return false;
        }

        hr = transform_->SetOutputType(output_stream_id_, output_type.Get(), 0);
        if (FAILED(hr)) {
            error = FriendlyNameForDiagnostics() + " SetOutputType(" +
                std::string(NativeVideoCodecName(codec_)) + ") failed: " + HResultString(hr);
            return false;
        }

        Microsoft::WRL::ComPtr<IMFMediaType> input_type;
        hr = CreateNv12InputType(&input_type);
        if (FAILED(hr)) {
            error = FriendlyNameForDiagnostics() + " input media type creation failed: " + HResultString(hr);
            return false;
        }

        hr = transform_->SetInputType(input_stream_id_, input_type.Get(), 0);
        if (FAILED(hr)) {
            error = FriendlyNameForDiagnostics() + " SetInputType(NV12) failed: " + HResultString(hr);
            return false;
        }

        hr = transform_->GetOutputStreamInfo(output_stream_id_, &output_info_);
        if (FAILED(hr)) {
            error = FriendlyNameForDiagnostics() + " GetOutputStreamInfo failed: " + HResultString(hr);
            return false;
        }

        (void)transform_->ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
        (void)transform_->ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        if (codec_ == NativeVideoCodec::H265) {
            LoadH265ParameterSetsFromOutputType();
        }
        last_error_.clear();
        return true;
    }

    // NVENC's HEVC MFT does not repeat VPS/SPS/PPS in the stream (live 2026-10-13: the key frame
    // arrived as a bare IDR); it publishes them as MF_MT_MPEG_SEQUENCE_HEADER on the output type.
    // Taken from there into the caches, so every key frame goes out decodable.
    void LoadH265ParameterSetsFromOutputType()
    {
        Microsoft::WRL::ComPtr<IMFMediaType> current;
        if (FAILED(transform_->GetOutputCurrentType(output_stream_id_, &current)) || !current) {
            return;
        }
        UINT32 size = 0;
        if (FAILED(current->GetBlobSize(MF_MT_MPEG_SEQUENCE_HEADER, &size)) || size == 0) {
            return;
        }
        std::vector<uint8_t> header(size);
        if (FAILED(current->GetBlob(MF_MT_MPEG_SEQUENCE_HEADER, header.data(), size, &size))) {
            return;
        }
        header.resize(size);
        (void)FinishH265Frame(header, false);
    }

public:
    /// Сменить разрешение НА МЕСТЕ, не пересоздавая трансформ.
    ///
    /// Разрешение задавалось только в `Initialize`, поэтому каждая ступень лестницы ABR стоила
    /// полного пересоздания сессии — перечисление MFT, активация, согласование типов, запуск
    /// потока. Это сотни миллисекунд видимой паузы; пользователь описывает их как «заминки»
    /// между ступенями качества.
    ///
    /// Media Foundation допускает смену типов на живом трансформе: остановить поток, поставить
    /// новый ВЫХОДНОЙ тип, затем ВХОДНОЙ (для кодировщика порядок именно такой), обновить
    /// `MFT_OUTPUT_STREAM_INFO` и запустить поток снова.
    ///
    /// Возврат false не беда: вызывающий пересоздаст кодировщик, как делал всегда. Поэтому правка
    /// не может сделать хуже, чем было.
    bool ReconfigureResolution(int width, int height, std::string& error)
    {
        if (!transform_) {
            error = "Media Foundation H.264 transform is null";
            return false;
        }
        if (width <= 0 || height <= 0) {
            error = "invalid resolution for reconfigure";
            return false;
        }
        if (width == width_ && height == height_) {
            return true;
        }

        // Сливаем то, что кодировщик ещё держит: смена типов на непустом конвейере — гонка.
        (void)transform_->ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
        (void)transform_->ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        (void)transform_->ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);

        const int previous_width = width_;
        const int previous_height = height_;
        width_ = width;
        height_ = height;

        std::string configure_error;
        if (!ConfigureTransform(configure_error)) {
            error = FriendlyNameForDiagnostics() + " in-place resize to " + std::to_string(width) +
                "x" + std::to_string(height) + " failed: " + configure_error;
            width_ = previous_width;
            height_ = previous_height;
            return false;
        }

        // Новый размер — новая последовательность параметров, значит следующий кадр обязан быть
        // ключевым: иначе приёмнику нечем будет начать декодирование.
        force_next_keyframe_ = true;
        cached_sps_.clear();
        cached_pps_.clear();
        ++resolution_changes_;
        return true;
    }

    uint64_t ResolutionChanges() const
    {
        return resolution_changes_;
    }

private:

    void UnlockAsyncTransform()
    {
        Microsoft::WRL::ComPtr<IMFAttributes> attributes;
        if (FAILED(transform_->GetAttributes(&attributes)) || !attributes) {
            return;
        }

        UINT32 async = 0;
        if (SUCCEEDED(attributes->GetUINT32(MF_TRANSFORM_ASYNC, &async)) && async != 0) {
            (void)attributes->SetUINT32(MF_TRANSFORM_ASYNC_UNLOCK, TRUE);
            async_ = true;
            (void)transform_.As(&event_generator_);
        }
    }

    void ConfigureCodecPropertiesBeforeTypeSet()
    {
        SetCodecUInt32(CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_CBR);
        SetCodecUInt32(CODECAPI_AVEncCommonMeanBitRate, static_cast<uint32_t>(bitrate_bps_));
        SetCodecUInt32(CODECAPI_AVEncCommonMaxBitRate, static_cast<uint32_t>(bitrate_bps_));
        // Keyframes ON DEMAND, not on a timer.
        //
        // This was fps*2 — an IDR every two seconds, for a 1:1 call that has NACK retransmission
        // AND PLI. Nothing asked for those keyframes (measured on a live call: 23 IDR in 45 s,
        // pli sent = 0), and every one of them is a frame several times the size of a P-frame
        // pushed onto the wire at once. Measured A/B on the same screen share, back to back:
        // 535 kbps with the periodic IDR against 275 kbps without, at the same 32 fps.
        //
        // What replaces it: the encoder emits an IDR on its first frame and whenever it is
        // reconfigured, and the receiver asks — via PLI — when it starts (kReasonDecoderStarted),
        // when it cannot decode, when a frame's playout deadline expires, when NACK retries run
        // out, and when the SPS/PPS tracker says the stream is unusable.
        //
        // "Effectively infinite" rather than 0: for an MFT, 0 means "encoder decides", which is
        // exactly the decision we are taking back. An hour at the current frame rate is a legal
        // positive value no call will reach.
        //
        // See docs/decisions/keyframes-on-demand.md — including what this does NOT fix: video is
        // broken at 10% packet loss either way (docs/bugs/video-dies-on-packet-loss.md).
        SetCodecUInt32(CODECAPI_AVEncMPVGOPSize, static_cast<uint32_t>(std::max(1, fps_ * 3600)));
        SetCodecUInt32(CODECAPI_AVEncMPVDefaultBPictureCount, 0);
        SetCodecUInt32(CODECAPI_AVLowLatencyMode, 1);
        // Столько же слоёв, сколько и у программного пути (см. BuildSmokeCodec): поведение под
        // затором не должно зависеть от того, на каком кодировщике оказался пользователь. У MFT
        // цена слоёв при этом ГОРАЗДО выше — 10.7 дБ против 0.7, — что и есть главная причина, по
        // которой они пока выключены у обоих.
        SetCodecUInt32(CODECAPI_AVEncVideoTemporalLayerCount, static_cast<uint32_t>(kTemporalLayers));
    }

    HRESULT CreateH264OutputType(IMFMediaType** media_type) const
    {
        Microsoft::WRL::ComPtr<IMFMediaType> type;
        HRESULT hr = MFCreateMediaType(&type);
        if (FAILED(hr)) {
            return hr;
        }

        hr = type->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video);
        if (FAILED(hr)) {
            return hr;
        }
        hr = type->SetGUID(MF_MT_SUBTYPE, OutputSubtype());
        if (FAILED(hr)) {
            return hr;
        }
        hr = MFSetAttributeSize(type.Get(), MF_MT_FRAME_SIZE, static_cast<UINT32>(width_), static_cast<UINT32>(height_));
        if (FAILED(hr)) {
            return hr;
        }
        hr = MFSetAttributeRatio(type.Get(), MF_MT_FRAME_RATE, static_cast<UINT32>(fps_), 1);
        if (FAILED(hr)) {
            return hr;
        }
        hr = MFSetAttributeRatio(type.Get(), MF_MT_PIXEL_ASPECT_RATIO, 1, 1);
        if (FAILED(hr)) {
            return hr;
        }
        hr = type->SetUINT32(MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive);
        if (FAILED(hr)) {
            return hr;
        }
        hr = type->SetUINT32(MF_MT_AVG_BITRATE, static_cast<UINT32>(bitrate_bps_));
        if (FAILED(hr)) {
            return hr;
        }
        // ПРОФИЛЬ. Здесь стоял Baseline — профиль без CABAC и без 8x8-преобразования, то есть
        // заведомо худшее сжатие при том же битрейте. Совместимость его не оправдывает: оба конца
        // наши, а High декодирует любое железо с поддержкой H.264 (это мейнстрим-профиль всего
        // видео, а не расширение). Меряется это стендом picture-quality (PSNR при фиксированном
        // битрейте), а не предположением.
        //
        // Если MFT профиль не примет, SetOutputType откажет, и ConfigureTransform повторит попытку
        // на ступень ниже — молча деградировать в качестве лучше, чем остаться без кодировщика.
        (void)type->SetUINT32(MF_MT_MPEG2_PROFILE, requested_profile_);

        *media_type = type.Detach();
        return S_OK;
    }

    HRESULT CreateNv12InputType(IMFMediaType** media_type) const
    {
        Microsoft::WRL::ComPtr<IMFMediaType> type;
        HRESULT hr = MFCreateMediaType(&type);
        if (FAILED(hr)) {
            return hr;
        }

        hr = type->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video);
        if (FAILED(hr)) {
            return hr;
        }
        hr = type->SetGUID(MF_MT_SUBTYPE, MFVideoFormat_NV12);
        if (FAILED(hr)) {
            return hr;
        }
        hr = MFSetAttributeSize(type.Get(), MF_MT_FRAME_SIZE, static_cast<UINT32>(width_), static_cast<UINT32>(height_));
        if (FAILED(hr)) {
            return hr;
        }
        hr = MFSetAttributeRatio(type.Get(), MF_MT_FRAME_RATE, static_cast<UINT32>(fps_), 1);
        if (FAILED(hr)) {
            return hr;
        }
        hr = MFSetAttributeRatio(type.Get(), MF_MT_PIXEL_ASPECT_RATIO, 1, 1);
        if (FAILED(hr)) {
            return hr;
        }
        hr = type->SetUINT32(MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive);
        if (FAILED(hr)) {
            return hr;
        }

        *media_type = type.Detach();
        return S_OK;
    }

    bool IsCodecPropertyModifiable(const GUID& property)
    {
        Microsoft::WRL::ComPtr<ICodecAPI> codec_api;
        if (FAILED(transform_.As(&codec_api)) || !codec_api) {
            return false;
        }

        return codec_api->IsModifiable(&property) == S_OK;
    }

    bool SetCodecUInt32(const GUID& property, uint32_t value)
    {
        Microsoft::WRL::ComPtr<ICodecAPI> codec_api;
        if (FAILED(transform_.As(&codec_api)) || !codec_api) {
            return false;
        }

        VARIANT variant;
        VariantInit(&variant);
        variant.vt = VT_UI4;
        variant.ulVal = value;
        const HRESULT hr = codec_api->SetValue(&property, &variant);
        VariantClear(&variant);
        return SUCCEEDED(hr);
    }

    /// Забрать у кодировщика новый тип выхода и поставить его — ответ на MF_E_TRANSFORM_STREAM_CHANGE.
    /// Порядок по контракту: перебрать доступные типы, поставить первый принятый, затем обновить
    /// MFT_OUTPUT_STREAM_INFO (размер буфера мог измениться вместе с типом).
    bool RenegotiateOutputType()
    {
        for (DWORD index = 0;; ++index) {
            Microsoft::WRL::ComPtr<IMFMediaType> candidate;
            HRESULT hr = transform_->GetOutputAvailableType(output_stream_id_, index, &candidate);
            if (hr == MF_E_NO_MORE_TYPES) {
                last_error_ = name_ + " offered no output type after a stream change";
                return false;
            }
            if (FAILED(hr)) {
                last_error_ = "GetOutputAvailableType failed after stream change: " + HResultString(hr);
                return false;
            }
            if (SUCCEEDED(transform_->SetOutputType(output_stream_id_, candidate.Get(), 0))) {
                hr = transform_->GetOutputStreamInfo(output_stream_id_, &output_info_);
                if (FAILED(hr)) {
                    last_error_ =
                        "GetOutputStreamInfo failed after stream change: " + HResultString(hr);
                    return false;
                }
                return true;
            }
        }
    }


    OmniWebRtcMediaStatus TryProcessOutput(uint32_t rtp_timestamp, EncodedFrame& encoded)
    {
        encoded = {};
        if (!transform_) {
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }

        // У асинхронного MFT ProcessOutput можно звать ТОЛЬКО после METransformHaveOutput.
        // Ожидание этого события теперь ВЫШЕ — в DrainAvailableOutputs, вместе с ожиданием
        // METransformNeedInput. Держать его здесь было нельзя: функция ждала одно событие и
        // выбрасывала остальные, из-за чего терялось «пора подать вход» и кодировщик навсегда
        // переставал принимать кадры.
        Microsoft::WRL::ComPtr<IMFSample> output_sample;
        Microsoft::WRL::ComPtr<IMFSample> allocated_sample;
        MFT_OUTPUT_DATA_BUFFER output_data{};
        output_data.dwStreamID = output_stream_id_;
        if ((output_info_.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES) == 0) {
            HRESULT hr = MFCreateSample(&allocated_sample);
            if (FAILED(hr)) {
                last_error_ = "MFCreateSample(output) failed: " + HResultString(hr);
                return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
            }

            Microsoft::WRL::ComPtr<IMFMediaBuffer> output_buffer;
            const DWORD output_size = std::max<DWORD>(
                output_info_.cbSize,
                static_cast<DWORD>(std::max(64 * 1024, width_ * height_)));
            hr = MFCreateMemoryBuffer(output_size, &output_buffer);
            if (FAILED(hr)) {
                last_error_ = "MFCreateMemoryBuffer(output) failed: " + HResultString(hr);
                return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
            }

            hr = allocated_sample->AddBuffer(output_buffer.Get());
            if (FAILED(hr)) {
                last_error_ = "AddBuffer(output) failed: " + HResultString(hr);
                return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
            }

            output_data.pSample = allocated_sample.Get();
        }

        DWORD process_status = 0;
        HRESULT hr = transform_->ProcessOutput(0, 1, &output_data, &process_status);
        if (output_data.pEvents != nullptr) {
            output_data.pEvents->Release();
            output_data.pEvents = nullptr;
        }

        if (hr == MF_E_TRANSFORM_NEED_MORE_INPUT) {
            return OMNI_WEBRTC_MEDIA_STATUS_OK;
        }

        if (hr == MF_E_TRANSFORM_STREAM_CHANGE) {
            // Это НЕ ошибка, а штатное требование контракта MFT: кодировщик пересогласовал тип
            // выхода и ждёт, что мы его заберём и поставим. Аппаратные кодировщики Intel делают
            // это регулярно после первых кадров. Пока код считал событие фатальным, production-
            // сессия объявлялась несостоявшейся — а вместе с ней отваливалось ВСЁ видео, потому
            // что список кодеков нативной сессии наполняется только при удачной аппаратной сессии.
            if (renegotiating_output_) {
                // Второй раз подряд на одном кадре не пересогласовываем: это была бы петля.
                return OMNI_WEBRTC_MEDIA_STATUS_OK;
            }
            if (!RenegotiateOutputType()) {
                return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
            }
            // Повторяем попытку сразу: пересогласование случается на ПЕРВОМ же кадре, и без
            // повтора этот кадр молча пропал бы. Именно на этом спотыкался дымовой тест, который
            // подаёт один кадр и сразу ждёт RTP.
            renegotiating_output_ = true;
            const OmniWebRtcMediaStatus retried = TryProcessOutput(rtp_timestamp, encoded);
            renegotiating_output_ = false;
            return retried;
        }

        if (FAILED(hr)) {
            last_error_ = name_ + " ProcessOutput failed: " + HResultString(hr);
            ++encode_failures_;
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }

        if ((output_info_.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES) != 0) {
            output_sample.Attach(output_data.pSample);
            output_data.pSample = nullptr;
        } else {
            output_sample = allocated_sample;
        }

        if (!output_sample) {
            last_error_ = name_ + " ProcessOutput returned no sample";
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }

        Microsoft::WRL::ComPtr<IMFMediaBuffer> output_buffer;
        hr = output_sample->ConvertToContiguousBuffer(&output_buffer);
        if (FAILED(hr) || !output_buffer) {
            last_error_ = "ConvertToContiguousBuffer(output) failed: " + HResultString(hr);
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }

        BYTE* source = nullptr;
        DWORD max_len = 0;
        DWORD current_len = 0;
        hr = output_buffer->Lock(&source, &max_len, &current_len);
        if (FAILED(hr) || source == nullptr) {
            last_error_ = "output buffer Lock failed: " + HResultString(hr);
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }

        if (current_len > 0 && codec_ != NativeVideoCodec::H264) {
            encoded.data.assign(source, source + current_len);
            UINT32 clean_point = 0;
            (void)output_sample->GetUINT32(MFSampleExtension_CleanPoint, &clean_point);
            const bool keyframe = codec_ == NativeVideoCodec::H265
                ? FinishH265Frame(encoded.data, clean_point != 0)
                : FinishAv1Frame(encoded.data, clean_point != 0);
            // H.265 QP is parsed by libwebrtc itself (QpParser); AV1 has no such parser there.
            if (codec_ == NativeVideoCodec::Av1) {
                const std::optional<int> qp = av1_qp_parser_.Parse(encoded.data.data(), encoded.data.size());
                if (qp.has_value()) {
                    encoded.qp = static_cast<uint8_t>(*qp);
                }
            }
            if (keyframe) {
                ++idr_frames_;
            }
            encoded.rtp_timestamp = rtp_timestamp;
            encoded.frame_type = keyframe
                ? webrtc::VideoFrameType::kVideoFrameKey
                : webrtc::VideoFrameType::kVideoFrameDelta;
        } else if (current_len > 0) {
            encoded.data.assign(source, source + current_len);
            NormalizeH264AnnexB(encoded.data);
            RefreshH264ParameterSetCache(encoded.data);
            EnsureH264ParameterSetsForKeyframe(encoded.data);
            const bool has_idr = ContainsH264Idr(encoded.data);
            if (has_idr) {
                ++idr_frames_;
            }
            if (ContainsH264NaluType(encoded.data, static_cast<uint8_t>(webrtc::H264::NaluType::kSps))) {
                ++sps_frames_;
            }
            if (ContainsH264NaluType(encoded.data, static_cast<uint8_t>(webrtc::H264::NaluType::kPps))) {
                ++pps_frames_;
            }
            encoded.rtp_timestamp = rtp_timestamp;
            encoded.frame_type = has_idr
                ? webrtc::VideoFrameType::kVideoFrameKey
                : webrtc::VideoFrameType::kVideoFrameDelta;
        }

        (void)output_buffer->Unlock();
        return OMNI_WEBRTC_MEDIA_STATUS_OK;
    }

    /// СОБЫТИЯ АСИНХРОННОГО MFT НАДО СЧИТАТЬ, А НЕ ВЫБРАСЫВАТЬ.
    ///
    /// Здесь была функция, которая крутила очередь событий в поисках ОДНОГО ожидаемого типа и
    /// молча теряла все остальные. Ожидая METransformHaveOutput, она уничтожала
    /// METransformNeedInput — а асинхронный MFT принимает вход ТОЛЬКО после него. Дальше
    /// `ProcessInput` навсегда отвечал `MF_E_NOTACCEPTING` (0xC00D36B5).
    ///
    /// Живой замер 2026-08-27: за звонок кодировщик отдал ОДИН годный кадр из 906 поданных,
    /// каждый вызов стоил ~360 мс ожидания события, которого уже не будет, и у собеседника
    /// умирала картинка (docs/bugs/encoder-goes-silent-mid-call.md).
    ///
    /// Теперь очередь разбирается целиком, а типы копятся счётчиками: ни одно событие не
    /// пропадает, и оба конца контракта — «пора подать вход» и «готов выход» — видны каждому,
    /// кто их ждёт.
    void PumpAsyncEvents()
    {
        if (!event_generator_) {
            return;
        }
        for (;;) {
            Microsoft::WRL::ComPtr<IMFMediaEvent> event;
            const HRESULT hr = event_generator_->GetEvent(MF_EVENT_FLAG_NO_WAIT, &event);
            if (hr == MF_E_NO_EVENTS_AVAILABLE || FAILED(hr) || !event) {
                return;
            }
            MediaEventType type = MEUnknown;
            if (!SUCCEEDED(event->GetType(&type))) {
                continue;
            }
            if (type == METransformNeedInput) {
                ++pending_need_input_;
            } else if (type == METransformHaveOutput) {
                ++pending_have_output_;
            }
        }
    }

    /// Забрать одно накопленное событие нужного типа, дождавшись его не дольше timeout.
    /// Нулевой timeout = «только то, что уже пришло».
    bool TakeAsyncEvent(MediaEventType expected, std::chrono::milliseconds timeout)
    {
        int* counter = expected == METransformNeedInput ? &pending_need_input_
                     : expected == METransformHaveOutput ? &pending_have_output_
                     : nullptr;
        if (counter == nullptr) {
            return false;
        }
        const auto deadline = std::chrono::steady_clock::now() + timeout;
        for (;;) {
            PumpAsyncEvents();
            if (*counter > 0) {
                --*counter;
                return true;
            }
            if (std::chrono::steady_clock::now() >= deadline) {
                return false;
            }
            std::this_thread::sleep_for(std::chrono::milliseconds(1));
        }
    }

    /// Выбрать ВСЕ готовые выходы. Контракт MFT: пока выход не выбран, вход не принимается.
    ///
    /// Прежний код выбирал ровно один раз, а в ветке восстановления после NOTACCEPTING — выбирал
    /// и ВЫБРАСЫВАЛ полученный кадр. То есть кадр, который кодировщик уже сделал, не доезжал
    /// никуда.
    void DrainAvailableOutputs(uint32_t rtp_timestamp)
    {
        for (int guard = 0; guard < 8; ++guard) {
            if (async_) {
                // Первому выходу даём подождать (кодировщик конвейерный), следующим — нет:
                // они либо уже готовы, либо их нет.
                const auto timeout = guard == 0 ? std::chrono::milliseconds(250)
                                                : std::chrono::milliseconds(0);
                if (!TakeAsyncEvent(METransformHaveOutput, timeout)) {
                    return;
                }
            }
            EncodedFrame frame;
            if (TryProcessOutput(rtp_timestamp, frame) != OMNI_WEBRTC_MEDIA_STATUS_OK) {
                return;
            }
            if (frame.data.empty()) {
                return;  // MF_E_TRANSFORM_NEED_MORE_INPUT — выходов больше нет
            }
            pending_outputs_.push_back(std::move(frame));
        }
    }

    size_t Nv12FrameSize() const
    {
        return static_cast<size_t>(width_) * static_cast<size_t>(height_) +
            static_cast<size_t>(width_) * static_cast<size_t>((height_ + 1) / 2);
    }

    void CopyNv12Planes(
        uint8_t* target,
        const uint8_t* y_plane,
        int32_t y_stride,
        const uint8_t* uv_plane,
        int32_t uv_stride) const
    {
        for (int y = 0; y < height_; ++y) {
            std::memcpy(
                target + static_cast<size_t>(y) * static_cast<size_t>(width_),
                y_plane + static_cast<size_t>(y) * static_cast<size_t>(y_stride),
                static_cast<size_t>(width_));
        }

        uint8_t* uv_target = target + static_cast<size_t>(width_) * static_cast<size_t>(height_);
        const int chroma_height = (height_ + 1) / 2;
        for (int y = 0; y < chroma_height; ++y) {
            std::memcpy(
                uv_target + static_cast<size_t>(y) * static_cast<size_t>(width_),
                uv_plane + static_cast<size_t>(y) * static_cast<size_t>(uv_stride),
                static_cast<size_t>(width_));
        }
    }

    LONGLONG FrameDuration100Ns() const
    {
        return static_cast<LONGLONG>(10'000'000 / std::max(1, fps_));
    }

    static std::string ReadFriendlyName(IMFActivate* activate)
    {
        wchar_t* friendly_name = nullptr;
        UINT32 friendly_name_length = 0;
        if (activate != nullptr &&
            SUCCEEDED(activate->GetAllocatedString(
                MFT_FRIENDLY_NAME_Attribute,
                &friendly_name,
                &friendly_name_length)) &&
            friendly_name != nullptr &&
            friendly_name_length > 0) {
            std::string name = WideToUtf8(friendly_name);
            CoTaskMemFree(friendly_name);
            return name;
        }

        if (friendly_name != nullptr) {
            CoTaskMemFree(friendly_name);
        }

        return "Media Foundation hardware H.264 encoder";
    }

    // Best-effort PCI vendor id from a HW encoder MFT's friendly name. MF does not expose a
    // reliable per-MFT vendor attribute across versions, but the friendly name is vendor-stamped
    // ("NVIDIA H.264 Encoder MFT", "Intel(R) Quick Sync ...", "AMD H.264 Encoder"). Used only to
    // route the capability probe's functional encode test to a specific adapter; 0 = unknown.
    static UINT VendorIdFromFriendlyName(const std::string& name)
    {
        std::string lower;
        lower.reserve(name.size());
        for (char c : name) {
            lower.push_back(static_cast<char>(std::tolower(static_cast<unsigned char>(c))));
        }
        if (lower.find("nvidia") != std::string::npos) {
            return 0x10DEu;
        }
        if (lower.find("intel") != std::string::npos || lower.find("quick sync") != std::string::npos) {
            return 0x8086u;
        }
        if (lower.find("amd") != std::string::npos || lower.find("radeon") != std::string::npos) {
            return 0x1002u;
        }
        return 0u;
    }

    std::string FriendlyNameForDiagnostics() const
    {
        return name_.empty()
            ? "Media Foundation hardware " + std::string(NativeVideoCodecName(codec_)) + " encoder"
            : name_;
    }

    GUID OutputSubtype() const
    {
        switch (codec_) {
        case NativeVideoCodec::H265:
            return MFVideoFormat_HEVC;
        case NativeVideoCodec::Av1:
            return MFVideoFormat_AV1;
        case NativeVideoCodec::H264:
        default:
            return MFVideoFormat_H264;
        }
    }

    // HEVC: a key frame carries VPS/SPS/PPS so a receiver joining at it can decode. MFTs emit them
    // with the first IDR only (the H.264 path below learned this the hard way), so they are
    // cached and put back in front of any later key frame that lacks them. Returns "key frame".
    bool FinishH265Frame(std::vector<uint8_t>& data, bool clean_point)
    {
        // An access unit delimiter (type 35) is optional; it must come first in the unit, and the
        // parameter sets put back below must come before the IDR — so it goes.
        {
            const auto nalus = webrtc::H264::FindNaluIndices(data);
            if (!nalus.empty() && nalus[0].payload_size > 0 &&
                nalus[0].payload_start_offset < data.size() &&
                ((data[nalus[0].payload_start_offset] >> 1) & 0x3Fu) == 35u) {
                const size_t next = nalus.size() > 1 ? nalus[1].start_offset : data.size();
                data.erase(data.begin(), data.begin() + static_cast<ptrdiff_t>(next));
            }
        }
        bool irap = false;
        bool has_vps = false;
        bool has_sps = false;
        bool has_pps = false;
        for (const auto& nalu : webrtc::H264::FindNaluIndices(data)) {
            if (nalu.payload_size == 0 || nalu.payload_start_offset >= data.size()) {
                continue;
            }
            const uint8_t type = (data[nalu.payload_start_offset] >> 1) & 0x3Fu;
            std::vector<uint8_t>* cache = nullptr;
            if (type == 32u) {
                has_vps = true;
                cache = &cached_vps_;
            } else if (type == 33u) {
                has_sps = true;
                cache = &cached_sps_;
            } else if (type == 34u) {
                has_pps = true;
                cache = &cached_pps_;
            } else if (type >= 16u && type <= 21u) {
                irap = true;
            }
            if (cache != nullptr) {
                std::vector<uint8_t> copy = CopyH264NaluPayload(data, nalu);
                if (!copy.empty()) {
                    *cache = std::move(copy);
                }
            }
        }
        const bool keyframe = irap || clean_point;
        if (keyframe && !(has_vps && has_sps && has_pps) && !cached_vps_.empty() &&
            !cached_sps_.empty() && !cached_pps_.empty()) {
            std::vector<uint8_t> prefixed;
            prefixed.reserve(data.size() + cached_vps_.size() + cached_sps_.size() +
                             cached_pps_.size() + 12);
            for (const std::vector<uint8_t>* set : {&cached_vps_, &cached_sps_, &cached_pps_}) {
                AppendH264LongStartCode(prefixed);
                prefixed.insert(prefixed.end(), set->begin(), set->end());
            }
            prefixed.insert(prefixed.end(), data.begin(), data.end());
            data = std::move(prefixed);
            ++parameter_set_prepends_;
        }
        return keyframe;
    }

    // The NVENC AV1 MFT may hand its output in an IVF container (seen live 2026-10-13, not in the
    // short probe): the first sample opens with the 32-byte file header "DKIF…AV01", and every
    // frame carries a 12-byte frame header (le32 size + le64 timestamp). RTP carries bare OBUs, so
    // both go; left in, they reached the receiver as 12 bytes of "garbage" in front of each frame
    // and dav1d refused every key frame.
    static void StripIvfFraming(std::vector<uint8_t>& data)
    {
        if (data.size() >= 32 && data[0] == 'D' && data[1] == 'K' && data[2] == 'I' && data[3] == 'F') {
            const size_t header = static_cast<size_t>(data[6]) | (static_cast<size_t>(data[7]) << 8);
            data.erase(data.begin(), data.begin() + static_cast<ptrdiff_t>(std::min(data.size(), header == 0 ? size_t{32} : header)));
        }
        if (data.size() > 12) {
            const size_t frame_size = static_cast<size_t>(data[0]) | (static_cast<size_t>(data[1]) << 8) |
                (static_cast<size_t>(data[2]) << 16) | (static_cast<size_t>(data[3]) << 24);
            if (frame_size == data.size() - 12) {
                data.erase(data.begin(), data.begin() + 12);
            }
        }
    }

    // AV1: OBUs in the low-overhead format (each with its size field). A key frame needs the
    // sequence header OBU in front; cached and put back when the MFT leaves it out. The RTP
    // packetizer drops temporal delimiters itself. Returns "key frame".
    bool FinishAv1Frame(std::vector<uint8_t>& data, bool clean_point)
    {
        StripIvfFraming(data);
        bool has_sequence_header = false;
        size_t offset = 0;
        while (offset < data.size()) {
            const uint8_t header = data[offset];
            const uint8_t type = (header >> 3) & 0x0Fu;
            const bool has_extension = (header & 0x04u) != 0;
            const bool has_size = (header & 0x02u) != 0;
            size_t cursor = offset + 1 + (has_extension ? 1 : 0);
            if (!has_size || cursor > data.size()) {
                break;  // not the low-overhead format: pass the frame through as it is
            }
            uint64_t size = 0;
            int shift = 0;
            bool size_ok = false;
            while (cursor < data.size() && shift < 56) {
                const uint8_t byte = data[cursor++];
                size |= static_cast<uint64_t>(byte & 0x7Fu) << shift;
                shift += 7;
                if ((byte & 0x80u) == 0) {
                    size_ok = true;
                    break;
                }
            }
            if (!size_ok || cursor + size > data.size()) {
                break;
            }
            const size_t end = cursor + static_cast<size_t>(size);
            if (type == 1u) {
                has_sequence_header = true;
                cached_sps_.assign(data.begin() + static_cast<ptrdiff_t>(offset),
                                   data.begin() + static_cast<ptrdiff_t>(end));
            }
            offset = end;
        }
        if (clean_point && !has_sequence_header && !cached_sps_.empty()) {
            data.insert(data.begin(), cached_sps_.begin(), cached_sps_.end());
            ++parameter_set_prepends_;
        }
        return clean_point;
    }

    static bool ContainsH264Idr(const std::vector<uint8_t>& data)
    {
        for (const auto& nalu : webrtc::H264::FindNaluIndices(data)) {
            if (nalu.payload_size == 0) {
                continue;
            }

            const uint8_t nalu_type = data[nalu.payload_start_offset] & 0x1Fu;
            if (nalu_type == 5u) {
                return true;
            }
        }

        return false;
    }

    static bool ContainsH264NaluType(const std::vector<uint8_t>& data, uint8_t expected_type)
    {
        for (const auto& nalu : webrtc::H264::FindNaluIndices(data)) {
            if (nalu.payload_size == 0) {
                continue;
            }

            const uint8_t nalu_type = data[nalu.payload_start_offset] & 0x1Fu;
            if (nalu_type == expected_type) {
                return true;
            }
        }

        return false;
    }

    static std::vector<uint8_t> CopyH264NaluPayload(
        const std::vector<uint8_t>& data,
        const webrtc::H264::NaluIndex& nalu)
    {
        if (nalu.payload_size == 0 ||
            nalu.payload_start_offset >= data.size() ||
            nalu.payload_start_offset + nalu.payload_size > data.size()) {
            return {};
        }

        const auto begin = data.begin() + static_cast<ptrdiff_t>(nalu.payload_start_offset);
        const auto end = begin + static_cast<ptrdiff_t>(nalu.payload_size);
        return std::vector<uint8_t>(begin, end);
    }

    static void AppendH264LongStartCode(std::vector<uint8_t>& target)
    {
        target.insert(target.end(), {0, 0, 0, 1});
    }

    void RefreshH264ParameterSetCache(const std::vector<uint8_t>& data)
    {
        for (const auto& nalu : webrtc::H264::FindNaluIndices(data)) {
            if (nalu.payload_size == 0) {
                continue;
            }

            const uint8_t nalu_type = data[nalu.payload_start_offset] & 0x1Fu;
            if (nalu_type == static_cast<uint8_t>(webrtc::H264::NaluType::kSps)) {
                std::vector<uint8_t> sps = CopyH264NaluPayload(data, nalu);
                if (!sps.empty()) {
                    cached_sps_ = std::move(sps);
                }
            } else if (nalu_type == static_cast<uint8_t>(webrtc::H264::NaluType::kPps)) {
                std::vector<uint8_t> pps = CopyH264NaluPayload(data, nalu);
                if (!pps.empty()) {
                    cached_pps_ = std::move(pps);
                }
            }
        }
    }

    void EnsureH264ParameterSetsForKeyframe(std::vector<uint8_t>& data)
    {
        if (!ContainsH264Idr(data)) {
            return;
        }

        const bool has_sps = ContainsH264NaluType(
            data,
            static_cast<uint8_t>(webrtc::H264::NaluType::kSps));
        const bool has_pps = ContainsH264NaluType(
            data,
            static_cast<uint8_t>(webrtc::H264::NaluType::kPps));
        if ((has_sps && has_pps) || cached_sps_.empty() || cached_pps_.empty()) {
            return;
        }

        std::vector<uint8_t> prefixed;
        prefixed.reserve(
            data.size() +
            cached_sps_.size() +
            cached_pps_.size() +
            (2u * webrtc::H264::kNaluLongStartSequenceSize));
        AppendH264LongStartCode(prefixed);
        prefixed.insert(prefixed.end(), cached_sps_.begin(), cached_sps_.end());
        AppendH264LongStartCode(prefixed);
        prefixed.insert(prefixed.end(), cached_pps_.begin(), cached_pps_.end());
        prefixed.insert(prefixed.end(), data.begin(), data.end());
        data = std::move(prefixed);
        ++parameter_set_prepends_;
    }

    static void NormalizeH264AnnexB(std::vector<uint8_t>& data)
    {
        if (!webrtc::H264::FindNaluIndices(data).empty()) {
            return;
        }

        if (data.size() < 4) {
            return;
        }

        std::vector<uint8_t> annex_b;
        size_t offset = 0;
        while (offset + 4u <= data.size()) {
            const uint32_t nalu_len = ReadBigEndian32(data.data() + offset);
            offset += 4u;
            if (nalu_len == 0 || offset + nalu_len > data.size()) {
                return;
            }

            annex_b.insert(annex_b.end(), {0, 0, 0, 1});
            annex_b.insert(annex_b.end(), data.begin() + static_cast<ptrdiff_t>(offset),
                data.begin() + static_cast<ptrdiff_t>(offset + nalu_len));
            offset += nalu_len;
        }

        if (offset == data.size() && !annex_b.empty()) {
            data = std::move(annex_b);
        }
    }

    Microsoft::WRL::ComPtr<IMFTransform> transform_;
    Microsoft::WRL::ComPtr<IMFMediaEventGenerator> event_generator_;
    MFT_OUTPUT_STREAM_INFO output_info_{};
    DWORD input_stream_id_ = kStreamId;
    DWORD output_stream_id_ = kStreamId;
    int width_ = 0;
    int height_ = 0;
    int fps_ = 30;
    int bitrate_bps_ = 800'000;
    // Atomic counters for MF hardware encoder. Mutated from caller's push
    // thread (e.g. EncodeNv12 from PushI420Frame / PushBgraFrame) and read
    // from diagnostics paths. See docs/decisions/native-media-locking.md.
    std::atomic<uint64_t> frames_submitted_{0};
    std::atomic<uint64_t> frames_encoded_{0};
    std::atomic<uint64_t> encode_failures_{0};
    std::atomic<uint64_t> rate_update_failures_{0};
    // -1 = ещё не спрашивали. См. IsCodecPropertyModifiable: «SetValue вернул S_OK» и «MFT это
    // применил» — разные утверждения, и различить их можно только спросив.
    std::atomic<int> mean_bitrate_modifiable_{-1};
    UINT32 requested_profile_ = eAVEncH264VProfile_High;
    NativeVideoCodec codec_ = NativeVideoCodec::H264;
    std::vector<uint8_t> cached_vps_;
    bool co_initialized_ = false;
    DWORD co_init_thread_ = 0;
    bool mf_started_ = false;
    bool async_ = false;
    /// Защита от петли: пересогласование типа выхода повторяет TryProcessOutput ровно один раз.
    bool renegotiating_output_ = false;
    /// Накопленные события асинхронного MFT — см. PumpAsyncEvents.
    int pending_need_input_ = 0;
    int pending_have_output_ = 0;
    /// Готовые кадры, которые кодировщик отдал сверх одного за вызов. Ни один не выбрасывается.
    std::deque<EncodedFrame> pending_outputs_;
    bool force_next_keyframe_ = false;
    uint64_t parameter_set_prepends_ = 0;
    /// Сколько раз разрешение сменилось БЕЗ пересоздания трансформа. Видно в диагностике: если
    /// счётчик стоит, а картинка всё равно замирает на ступенях — виноват не кодировщик.
    uint64_t resolution_changes_ = 0;
    uint64_t idr_frames_ = 0;
    uint64_t sps_frames_ = 0;
    uint64_t pps_frames_ = 0;
    std::vector<uint8_t> cached_sps_;
    Av1QpParser av1_qp_parser_;
    std::vector<uint8_t> cached_pps_;
    std::string name_;
    std::string last_error_;
    // Which MFT was taken and which were passed over, appended to errors (see Initialize).
    std::string selection_note_;
};

// АППАРАТНЫЙ КОДИРОВЩИК ЗА ИНТЕРФЕЙСОМ АПСТРИМА (#61, шаг D1).
//
// Зачем. У нас было ДВА пути кодирования, и выбирались они проверкой указателя на null:
// `hardware_h264_encoder_ ? ... : ...`. Каждая точка, где кодировщик что-то умеет — смена
// разрешения, скорость, ключевой кадр, статистика, — была написана дважды. Это ровно то
// накопление, о котором говорит CONTRIBUTING.md, и оно же — стена перед VideoStreamEncoder: тот умеет
// водить только webrtc::VideoEncoder и про наш MFT не знает ничего.
//
// Теперь путь один. Аппаратный кодировщик — такой же webrtc::VideoEncoder, как программный, и
// выбор между ними делает фабрика, а не проверка на null.
//
// ЧТО ЗДЕСЬ НЕ САМОДЕЛКА. Обёртка не содержит ни кодирования, ни разбора битового потока, ни
// подстановки наборов параметров — всё это внутри MediaFoundationH264Encoder и не тронуто. Здесь
// только перевод: VideoFrame -> NV12 для MFT и EncodedFrame -> EncodedImage для апстрима.
//
// ПРО has_trusted_rate_controller = false. Это не мелочь: у аппаратного H.264 нет флага пропуска
// кадров, и он систематически переливает через запрошенный битрейт (замер 2026-08-17: просили
// 150 кбит/с, отдавал 605). Признавшись, что его регулятору верить нельзя, мы получаем
// апстримовский EncoderBitrateAdjuster — тот самый, который у нас стоял отдельным полем
// bitrate_adjuster_ и вёл своё окно перелива.
class OmniHardwareH264Encoder final : public webrtc::VideoEncoder {
public:
    OmniHardwareH264Encoder(
        UINT required_vendor_id,
        std::shared_ptr<OmniEncoderIoStats> stats,
        int test_fail_after_frames,
        NativeVideoCodec codec = NativeVideoCodec::H264)
        : required_vendor_id_(required_vendor_id),
          io_stats_(std::move(stats)),
          test_fail_after_frames_(test_fail_after_frames)
    {
        codec_ = codec;
    }

    ~OmniHardwareH264Encoder() override = default;

    int32_t InitEncode(
        const webrtc::VideoCodec* codec_settings,
        const webrtc::VideoEncoder::Settings& settings) override
    {
        (void)settings;
        if (codec_settings == nullptr || codec_settings->width <= 0 ||
            codec_settings->height <= 0) {
            return WEBRTC_VIDEO_CODEC_ERR_PARAMETER;
        }

        // Новая настройка кодировщика — новый счёт отказов: прошлые относились к прошлому
        // состоянию устройства.
        consecutive_failures_ = 0;
        width_ = codec_settings->width;
        height_ = codec_settings->height;
        fps_ = codec_settings->maxFramerate > 0 ? codec_settings->maxFramerate : 30;
        bitrate_bps_ = static_cast<int>(codec_settings->startBitrate) * 1000;
        if (bitrate_bps_ <= 0) {
            bitrate_bps_ = 800'000;
        }

        // СМЕНА РАЗМЕРА — НА МЕСТЕ, А НЕ ПЕРЕСОЗДАНИЕМ.
        //
        // Апстрим меняет разрешение повторным InitEncode. У MFT пересоздание стоит сотен
        // миллисекунд видимой паузы («заминки» между ступенями качества), и ради этого в
        // MediaFoundationH264Encoder заведена ReconfigureResolution. Сохраняем её здесь.
        if (impl_) {
            std::string resize_error;
            if (impl_->ReconfigureResolution(width_, height_, resize_error)) {
                (void)impl_->SetRates(bitrate_bps_, fps_);
                return WEBRTC_VIDEO_CODEC_OK;
            }
            impl_.reset();
        }

        std::string error;
        impl_ = MediaFoundationH264Encoder::Create(
            width_, height_, fps_, bitrate_bps_, error, required_vendor_id_, codec_);
        if (!impl_) {
            last_error_ = error;
            return WEBRTC_VIDEO_CODEC_ERROR;
        }
        last_error_.clear();
        return WEBRTC_VIDEO_CODEC_OK;
    }

    int32_t RegisterEncodeCompleteCallback(webrtc::EncodedImageCallback* callback) override
    {
        callback_ = callback;
        return WEBRTC_VIDEO_CODEC_OK;
    }

    int32_t Release() override
    {
        impl_.reset();
        return WEBRTC_VIDEO_CODEC_OK;
    }

    int32_t Encode(
        const webrtc::VideoFrame& frame,
        const std::vector<webrtc::VideoFrameType>* frame_types) override
    {
        if (!impl_) {
            return WEBRTC_VIDEO_CODEC_UNINITIALIZED;
        }

        // MFT ест NV12 и только его. Кадр приходит либо уже в NV12 (путь захвата экрана и
        // камеры), либо в I420 (всё остальное) — во втором случае переводим через libyuv, тем же
        // вызовом, что стоял на этом пути и раньше.
        webrtc::scoped_refptr<webrtc::VideoFrameBuffer> buffer = frame.video_frame_buffer();
        if (!buffer) {
            return WEBRTC_VIDEO_CODEC_ERR_PARAMETER;
        }

        const uint8_t* y_plane = nullptr;
        int y_stride = 0;
        const uint8_t* uv_plane = nullptr;
        int uv_stride = 0;
        webrtc::scoped_refptr<webrtc::NV12Buffer> converted;
        if (buffer->type() == webrtc::VideoFrameBuffer::Type::kNV12) {
            const webrtc::NV12BufferInterface* nv12 = buffer->GetNV12();
            y_plane = nv12->DataY();
            y_stride = nv12->StrideY();
            uv_plane = nv12->DataUV();
            uv_stride = nv12->StrideUV();
        } else {
            webrtc::scoped_refptr<const webrtc::I420BufferInterface> i420 = buffer->ToI420();
            if (!i420) {
                return WEBRTC_VIDEO_CODEC_ERR_PARAMETER;
            }
            converted = webrtc::NV12Buffer::Create(i420->width(), i420->height());
            if (!converted) {
                return WEBRTC_VIDEO_CODEC_ERROR;
            }
            if (libyuv::I420ToNV12(
                    i420->DataY(), i420->StrideY(),
                    i420->DataU(), i420->StrideU(),
                    i420->DataV(), i420->StrideV(),
                    converted->MutableDataY(), converted->StrideY(),
                    converted->MutableDataUV(), converted->StrideUV(),
                    i420->width(), i420->height()) != 0) {
                return WEBRTC_VIDEO_CODEC_ERROR;
            }
            y_plane = converted->DataY();
            y_stride = converted->StrideY();
            uv_plane = converted->DataUV();
            uv_stride = converted->StrideUV();
        }

        const bool keyframe = frame_types != nullptr && !frame_types->empty() &&
                              (*frame_types)[0] == webrtc::VideoFrameType::kVideoFrameKey;

        // ВЫЗОВ СИНХРОННЫЙ, И ИМЕННО ПОЭТОМУ ЕГО НАДО МЕРИТЬ.
        //
        // Encode зовётся С ОЧЕРЕДИ КОДИРОВАНИЯ оркестратора. Пока он не вернулся, очередь занята,
        // и каждый новый кадр отвергается как kEncoderQueue. Значит «MFT задумался» и «кадры
        // выбрасываются пачками» — одно и то же событие, и увидеть его можно только здесь.
        EncodedFrame encoded;
        if (io_stats_) {
            io_stats_->encode_calls.fetch_add(1, std::memory_order_relaxed);
        }
        // ОТКАЗ УСТРОЙСТВА ПОСРЕДИ ЗВОНКА ВОСПРОИЗВЕСТИ ИНАЧЕ НЕЧЕМ.
        //
        // Драйвер, перезапуск GPU, потеря устройства — всё это бывает у пользователя и не бывает
        // в стенде. А проверять надо именно это: откат на процессор СОЗДАЁТСЯ правильно и
        // ПОСРЕДИ звонка не срабатывал (см. Encode ниже). Ключ ставит только стенд
        // (`test_hw_encode_fail_after` в конфигурации сессии); приложение его не передаёт
        // никогда, и по умолчанию его нет.
        const bool forced_failure =
            test_fail_after_frames_ > 0 && frames_seen_ >= test_fail_after_frames_;
        ++frames_seen_;
        const auto encode_started_at = std::chrono::steady_clock::now();
        const OmniWebRtcMediaStatus status =
            forced_failure
                ? OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR
                : impl_->EncodeNv12(
                      y_plane, y_stride, uv_plane, uv_stride,
                      frame.timestamp_us(), keyframe, frame.rtp_timestamp(), encoded);
        if (io_stats_) {
            const int64_t elapsed_ms = std::chrono::duration_cast<std::chrono::milliseconds>(
                std::chrono::steady_clock::now() - encode_started_at).count();
            io_stats_->encode_returns.fetch_add(1, std::memory_order_relaxed);
            io_stats_->last_encode_ms.store(elapsed_ms, std::memory_order_relaxed);
            int64_t previous_max = io_stats_->max_encode_ms.load(std::memory_order_relaxed);
            while (elapsed_ms > previous_max &&
                   !io_stats_->max_encode_ms.compare_exchange_weak(previous_max, elapsed_ms)) {
            }
            io_stats_->total_encode_ms.fetch_add(elapsed_ms, std::memory_order_relaxed);
            if (status != OMNI_WEBRTC_MEDIA_STATUS_OK) {
                io_stats_->encode_errors.fetch_add(1, std::memory_order_relaxed);
                std::lock_guard<std::mutex> lock(io_stats_->last_error_mutex);
                io_stats_->last_error = impl_ ? impl_->LastError() : std::string("(no impl)");
                if (io_stats_->last_error.empty()) {
                    io_stats_->last_error = "(encoder returned an error without a message)";
                }
            } else if (encoded.data.empty()) {
                io_stats_->encode_empty.fetch_add(1, std::memory_order_relaxed);
            } else {
                io_stats_->encode_ok.fetch_add(1, std::memory_order_relaxed);
            }
        }
        if (status != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            // ОБ ОТКАЗЕ НАДО ГОВОРИТЬ СЛОВОМ, КОТОРОЕ АПСТРИМ ПОНИМАЕТ.
            //
            // `VideoEncoderSoftwareFallbackWrapper` переключается на программный кодировщик
            // ровно по одному ответу — `WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE`
            // (video_encoder_software_fallback_wrapper.cc, EncodeWithMainEncoder). На
            // `WEBRTC_VIDEO_CODEC_ERROR` он не делает НИЧЕГО и продолжает звать нас дальше.
            //
            // Мы возвращали именно ERROR — и живьём это увидели: 41 отказ подряд, откат не
            // случился ни разу, у собеседника картинка умерла
            // (docs/bugs/encoder-goes-silent-mid-call.md). Отказ при СОЗДАНИИ обрабатывался
            // правильно (там обёртка откатывается сама на любой код), а отказ ПОСРЕДИ звонка —
            // драйвер, перезапуск GPU, потеря устройства — не обрабатывался вовсе.
            //
            // Почему не с первого отказа. Переключение одностороннее: назад на аппаратный
            // кодировщик обёртка уже не вернётся. Одиночный отказ бывает и разовым, а три
            // подряд — это не разовое; цена ошибки в эту сторону — три кадра (~100 мс), в
            // обратную — весь остаток звонка на процессоре.
            ++consecutive_failures_;
            if (consecutive_failures_ >= kConsecutiveFailuresBeforeFallback) {
                if (io_stats_) {
                    io_stats_->fallback_requests.fetch_add(1, std::memory_order_relaxed);
                }
                return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
            }
            return WEBRTC_VIDEO_CODEC_ERROR;
        }
        consecutive_failures_ = 0;
        // ПУСТОЙ ОТВЕТ — НЕ ОШИБКА. Аппаратный кодировщик асинхронен: кадр принят, а выйдет он на
        // одном из следующих. Вернуть здесь ошибку значило бы объявлять сбой на каждом таком
        // кадре — та же ловушка, что была на программном пути с пропуском кадров.
        if (encoded.data.empty() || callback_ == nullptr) {
            return WEBRTC_VIDEO_CODEC_OK;
        }

        webrtc::EncodedImage image;
        image.SetEncodedData(webrtc::EncodedImageBuffer::Create(
            encoded.data.data(), encoded.data.size()));
        image.SetRtpTimestamp(encoded.rtp_timestamp);
        image._frameType = encoded.frame_type;
        image._encodedWidth = static_cast<uint32_t>(width_);
        image._encodedHeight = static_cast<uint32_t>(height_);
        image.capture_time_ms_ = frame.timestamp_us() / 1000;
        image.SetSimulcastIndex(0);
        if (encoded.qp.has_value()) {
            image.qp_ = *encoded.qp;
        }

        webrtc::CodecSpecificInfo codec_specific;
        codec_specific.codecType = NativeVideoCodecType(codec_);
        if (codec_ == NativeVideoCodec::H264) {
            codec_specific.codecSpecific.H264.packetization_mode =
                webrtc::H264PacketizationMode::NonInterleaved;
            codec_specific.codecSpecific.H264.temporal_idx = webrtc::kNoTemporalIdx;
            codec_specific.codecSpecific.H264.base_layer_sync = false;
            codec_specific.codecSpecific.H264.idr_frame =
                encoded.frame_type == webrtc::VideoFrameType::kVideoFrameKey;
        }

        (void)callback_->OnEncodedImage(image, &codec_specific);
        return WEBRTC_VIDEO_CODEC_OK;
    }

    void SetRates(const RateControlParameters& parameters) override
    {
        if (!impl_) {
            return;
        }
        const int bitrate_bps = static_cast<int>(parameters.bitrate.get_sum_bps());
        const int fps = parameters.framerate_fps > 0.0
            ? static_cast<int>(std::lround(parameters.framerate_fps))
            : fps_;
        if (bitrate_bps > 0) {
            bitrate_bps_ = bitrate_bps;
        }
        if (fps > 0) {
            fps_ = fps;
        }
        (void)impl_->SetRates(bitrate_bps_, fps_);
    }

    EncoderInfo GetEncoderInfo() const override
    {
        EncoderInfo info;
        info.implementation_name = impl_
            ? impl_->Name()
            : "MediaFoundation " + std::string(NativeVideoCodecName(codec_));
        info.is_hardware_accelerated = true;
        info.supports_native_handle = false;
        // См. большой комментарий у класса: аппаратный H.264 переливает через запрошенный
        // битрейт, и признать это — единственный способ получить апстримовскую поправку.
        info.has_trusted_rate_controller = false;
        // QP — ИЗ САМОГО БИТСТРИМА, И ПО НЕМУ СУДИТ QualityScaler.
        //
        // MFT QP не сообщает (NVIDIA не ставит MFSampleExtension_VideoEncodeQP). Раньше здесь
        // стояло is_qp_trusted = false, и апстрим судил о размере кадра BandwidthQualityScaler'ом:
        // выход кодировщика против таблицы «битрейт на разрешение». Таблица есть только для H.264
        // и H.265, AV1 берёт H.264-ную, а на спокойной картинке кодировщик тратит меньше просто
        // потому, что ему не нужно больше. Живьём 2026-10-18: камера ноутбука, аппаратный AV1,
        // выход ~1,2 Мбит/с при цели 1,9 — скейлер прочёл «720p голодает» и ужал кадр до 990×556
        // (docs/bugs/hardware-encoder-shrank-calm-camera.md).
        //
        // QP говорит, насколько грубо закодирована картинка, а не сколько бит ушло. H.265
        // libwebrtc разбирает сам (VideoStreamEncoder: qp_ < 0 → QpParser), AV1 — Av1QpParser в
        // TryProcessOutput. Пороги — апстримовские для этих шкал: AV1 base_q_idx 145/205
        // (libaom_av1_encoder); у H.265 шкала та же, что у H.264 (0..51), своих порогов апстрим не
        // держит — берём H.264-ные 24/37 (h264_encoder_impl).
        //
        // H.264 ОСТАЁТСЯ НА ТАБЛИЦЕ. Аппаратный H.264 цели не слушается (см. класс: переливает), и
        // его QP не растёт, когда полосы мало: smoke abr-follow — цель 150 кбит/с, QP 24, выход
        // 885 кбит/с, кадр 720p не уменьшен. Таблица же написана как раз для H.264 и там права:
        // с ней тот же стенд уходит на 990×556 и 490 кбит/с.
        if (codec_ == NativeVideoCodec::H264) {
            info.is_qp_trusted = false;
        } else {
            info.is_qp_trusted = true;
            info.scaling_settings = codec_ == NativeVideoCodec::Av1
                ? ScalingSettings(kAv1LowQIndex, kAv1HighQIndex)
                : ScalingSettings(kH265LowQp, kH265HighQp);
        }
        info.supports_simulcast = false;
        return info;
    }

    /// Доступ к обёрнутому кодировщику — только для диагностики.
    MediaFoundationH264Encoder* impl() const { return impl_.get(); }
    const std::string& last_error() const { return last_error_; }

private:
    /// Сколько отказов подряд считаем поломкой устройства, а не помехой. См. Encode.
    static constexpr int kConsecutiveFailuresBeforeFallback = 3;
    /// QualityScaler thresholds, upstream's for these scales (see GetEncoderInfo).
    static constexpr int kH265LowQp = 24;
    static constexpr int kH265HighQp = 37;
    static constexpr int kAv1LowQIndex = 145;
    static constexpr int kAv1HighQIndex = 205;

    UINT required_vendor_id_ = 0;
    NativeVideoCodec codec_ = NativeVideoCodec::H264;
    std::shared_ptr<OmniEncoderIoStats> io_stats_;
    std::unique_ptr<MediaFoundationH264Encoder> impl_;
    int consecutive_failures_ = 0;
    int test_fail_after_frames_ = 0;
    int frames_seen_ = 0;
    webrtc::EncodedImageCallback* callback_ = nullptr;
    int width_ = 0;
    int height_ = 0;
    int fps_ = 30;
    int bitrate_bps_ = 0;
    std::string last_error_;
};

// ФАБРИКА КОДИРОВЩИКОВ: ОДИН ВЫБОР, И ОТКАТ НА ПРОЦЕССОР — АПСТРИМОВСКИЙ (#61, шаг D2a).
//
// Зачем фабрика вообще. VideoStreamEncoder не принимает готовый кодировщик — он просит его у
// фабрики, потому что умеет пересоздавать кодировщик сам (смена кодека, откат, переключение по
// запросу). Пока кодировщик строим мы, шага D2 не будет.
//
// И одна вещь чинится уже здесь. Наш откат на процессор был ОДНОРАЗОВЫМ: пробный InitEncode при
// создании сессии — не поднялся MFT, значит весь звонок идём процессором. Если же MFT умирал
// ПОСРЕДИ звонка (драйвер, перезапуск GPU, потеря устройства), видео просто вставало: откатываться
// было уже некому. У апстрима на это есть VideoEncoderSoftwareFallbackWrapper — он держит оба
// кодировщика и переключается на программный по факту отказа, в любой момент.
class OmniVideoEncoderFactory final : public webrtc::VideoEncoderFactory {
public:
    OmniVideoEncoderFactory(
        bool allow_hardware,
        uint32_t required_vendor_id,
        std::shared_ptr<OmniEncoderIoStats> stats,
        int test_fail_after_frames)
        : allow_hardware_(allow_hardware),
          required_vendor_id_(required_vendor_id),
          io_stats_(std::move(stats)),
          test_fail_after_frames_(test_fail_after_frames),
          builtin_(webrtc::CreateBuiltinVideoEncoderFactory())
    {
    }

    std::vector<webrtc::SdpVideoFormat> GetSupportedFormats() const override
    {
        std::vector<webrtc::SdpVideoFormat> formats =
            builtin_ ? builtin_->GetSupportedFormats() : std::vector<webrtc::SdpVideoFormat>{};
        // H.264 and HEVC have no software encoder in the build: they exist only as the hardware
        // MFT. The H.264 line is the one libwebrtc's own factory listed first when it still carried
        // OpenH264 (Baseline 3.1, packetization-mode 1 = FU-A), so packetizing is unchanged.
        if (allow_hardware_) {
            formats.push_back(webrtc::SdpVideoFormat(
                "H264",
                {{"level-asymmetry-allowed", "1"}, {"packetization-mode", "1"}, {"profile-level-id", "42001f"}}));
            formats.emplace_back("H265");
        }
        return formats;
    }

    std::unique_ptr<webrtc::VideoEncoder> Create(
        const webrtc::Environment& env,
        const webrtc::SdpVideoFormat& format) override
    {
        if (!builtin_) {
            return nullptr;
        }
        const std::optional<NativeVideoCodec> kind = ParseNativeVideoCodec(format.name);
        std::unique_ptr<webrtc::VideoEncoder> software =
            kind.has_value() && HasSoftwareVideoCodec(*kind) ? builtin_->Create(env, format) : nullptr;
#if defined(_WIN32)
        // The hardware MFT first, software behind it when there is one (AV1: libaom). H.264 and
        // HEVC have no software encoder: the hardware one alone.
        if (allow_hardware_ && kind.has_value()) {
            auto hardware = std::make_unique<OmniHardwareH264Encoder>(
                static_cast<UINT>(required_vendor_id_), io_stats_, test_fail_after_frames_, *kind);
            if (!software) {
                return hardware;
            }
            return webrtc::CreateVideoEncoderSoftwareFallbackWrapper(
                env, std::move(software), std::move(hardware),
                /*prefer_temporal_support=*/false);
        }
#endif
        return software;
    }

    // СЫРОЙ УКАЗАТЕЛЬ НА АППАРАТНЫЙ КОДИРОВЩИК УБРАН (остаток шага D2b).
    //
    // Он висел здесь вместе с методом hardware(), которого с того же шага никто не звал:
    // кодировщиком владеет VideoStreamEncoder и пересоздаёт его при каждой перенастройке, так
    // что указатель становился висячим — на этом уже был пойман ACCESS_VIOLATION в диагностике.
    // Всё, ради чего он был нужен, теперь приезжает через io_stats_ по shared_ptr.

private:
    bool allow_hardware_ = false;
    uint32_t required_vendor_id_ = 0;
    std::shared_ptr<OmniEncoderIoStats> io_stats_;
    int test_fail_after_frames_ = 0;
    std::unique_ptr<webrtc::VideoEncoderFactory> builtin_;
};

// HW H.264 decoder — Media Foundation / DXVA in D3D11 mode (task #84,
// docs/notes/hw-decode-CONTINUE.md). Mirrors MediaFoundationH264Encoder. Decodes H.264
// Annex-B access units into NV12 ID3D11Texture2D's ON THE CALLER'S D3D11 device, so the
// decoded frame stays a GPU surface and feeds D3D11VideoRenderer with no CPU round-trip
// (pairs with the GPU render path to make receive fully zero-copy). Vendor-neutral:
// MFTEnumEx(VIDEO_DECODER, HARDWARE) — the driver/MF map to NVDEC/QSV/etc. Create() returns
// null when no HW H.264 decoder MFT exists → caller keeps the software OpenH264 path
// (SW fallback is mandatory, docs/decisions/video-render-architecture.md). The caller's device
// MUST be created with D3D11_CREATE_DEVICE_VIDEO_SUPPORT + ID3D10Multithread protection.
class MediaFoundationH264Decoder {
public:
    // `codec`: H.264 or HEVC — the same DXVA decoder MFT contract, another input subtype. HEVC has
    // no software decoder in the build, so for it this decoder is the only way in
    // (docs/decisions/per-pair-video-codec-selection.md §9, phase 3).
    static std::unique_ptr<MediaFoundationH264Decoder> Create(
        ID3D11Device* device, int width, int height, std::string& error,
        NativeVideoCodec codec = NativeVideoCodec::H264)
    {
        std::unique_ptr<MediaFoundationH264Decoder> decoder(new MediaFoundationH264Decoder());
        decoder->codec_ = codec;
        if (!decoder->Initialize(device, width, height, error)) {
            return nullptr;
        }
        return decoder;
    }

    ~MediaFoundationH264Decoder()
    {
        if (transform_) {
            (void)transform_->ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            (void)transform_->ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            transform_.Reset();
        }
        event_generator_.Reset();
        device_manager_.Reset();
        if (mf_started_) {
            (void)MFShutdown();
        }
        // Only on the thread that initialised COM — see the encoder's destructor.
        if (co_initialized_ && co_init_thread_ == ::GetCurrentThreadId()) {
            CoUninitialize();
        }
    }

    // Feed one Annex-B access unit; drains at most one decoded NV12 frame. produced=true ⇒
    // out_nv12 + out_subresource describe a decoded NV12 texture on the caller's device (valid
    // until the next Decode call — copy/render before then). produced=false = decoder buffered
    // it / needs more input. Pixels never leave the GPU.
    OmniWebRtcMediaStatus Decode(
        const uint8_t* annexb,
        size_t len,
        int64_t timestamp_100ns,
        Microsoft::WRL::ComPtr<ID3D11Texture2D>& out_nv12,
        UINT& out_subresource,
        bool& produced)
    {
        produced = false;
        out_subresource = 0;
        if (!transform_ || annexb == nullptr || len == 0) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }

        Microsoft::WRL::ComPtr<IMFSample> input_sample;
        if (!WrapAnnexBSample(annexb, len, timestamp_100ns, input_sample)) {
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }

        if (async_) {
            (void)WaitForAsyncEvent(METransformNeedInput, std::chrono::milliseconds(100));
        }

        HRESULT hr = transform_->ProcessInput(0, input_sample.Get(), 0);
        if (hr == MF_E_NOTACCEPTING) {
            // Decoder is holding output — drain it, then retry the input once.
            const OmniWebRtcMediaStatus drained = DrainOneOutput(out_nv12, out_subresource, produced);
            if (drained != OMNI_WEBRTC_MEDIA_STATUS_OK || produced) {
                return drained;  // surfaced a frame (input retried on the next Decode call)
            }
            hr = transform_->ProcessInput(0, input_sample.Get(), 0);
        }
        if (FAILED(hr)) {
            last_error_ = "decoder ProcessInput failed: " + HResultString(hr);
            ++decode_failures_;
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }

        return DrainOneOutput(out_nv12, out_subresource, produced);
    }

    const std::string& Name() const { return name_; }
    const std::string& LastError() const { return last_error_; }
    uint64_t FramesDecoded() const { return frames_decoded_; }
    uint64_t DecodeFailures() const { return decode_failures_; }

private:
    bool Initialize(ID3D11Device* device, int width, int height, std::string& error)
    {
        if (device == nullptr) {
            error = "MediaFoundationH264Decoder: null D3D11 device";
            return false;
        }
        width_ = width;
        height_ = height;

        const HRESULT co_hr = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
        if (SUCCEEDED(co_hr)) {
            co_initialized_ = true;
            co_init_thread_ = ::GetCurrentThreadId();
        } else if (co_hr != RPC_E_CHANGED_MODE) {
            error = "CoInitializeEx failed: " + HResultString(co_hr);
            return false;
        }

        const HRESULT startup_hr = MFStartup(MF_VERSION, MFSTARTUP_LITE);
        if (FAILED(startup_hr)) {
            error = "MFStartup failed: " + HResultString(startup_hr);
            return false;
        }
        mf_started_ = true;

        HRESULT hr = MFCreateDXGIDeviceManager(&reset_token_, &device_manager_);
        if (FAILED(hr) || !device_manager_) {
            error = "MFCreateDXGIDeviceManager failed: " + HResultString(hr);
            return false;
        }
        hr = device_manager_->ResetDevice(device, reset_token_);
        if (FAILED(hr)) {
            error = "DXGI device manager ResetDevice failed (device needs VIDEO_SUPPORT + "
                "multithread protection): " + HResultString(hr);
            return false;
        }

        MFT_REGISTER_TYPE_INFO input_info;
        input_info.guidMajorType = MFMediaType_Video;
        input_info.guidSubtype = InputSubtype();
        // Filter only on the H.264 INPUT — decoders expose their output subtype (NV12) dynamically
        // after parsing the stream, not as a statically-registered output type.
        // Flags: include SYNCMFT|ASYNCMFT, not just HARDWARE. Unlike H.264 *encode* (real HW MFTs
        // like NVENC enumerate under HARDWARE), H.264 *decode* on Windows is done by the Microsoft
        // H264 Video Decoder MFT, which performs DXVA2/D3D11VA *hardware-accelerated* decode once a
        // D3D manager is set (SET_D3D_MANAGER below) — but it is NOT a HARDWARE-flagged MFT. So we
        // enumerate broadly + SORTANDFILTER (HW-capable first); the D3D11-texture (IMFDXGIBuffer)
        // output then PROVES the decode ran on the GPU (no DXVA ⇒ no DXGI buffer ⇒ we error out).
        IMFActivate** activates = nullptr;
        UINT32 activate_count = 0;
        const HRESULT enum_hr = MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_HARDWARE |
                MFT_ENUM_FLAG_SORTANDFILTER,
            &input_info,
            nullptr,
            &activates,
            &activate_count);
        if (FAILED(enum_hr) || activate_count == 0 || activates == nullptr) {
            error = "Media Foundation hardware H.264 decoder was not found: " + HResultString(enum_hr);
            if (activates != nullptr) {
                CoTaskMemFree(activates);
            }
            return false;
        }

        std::string last_error;
        for (UINT32 index = 0; index < activate_count; ++index) {
            Microsoft::WRL::ComPtr<IMFActivate> activate;
            activate.Attach(activates[index]);
            activates[index] = nullptr;
            if (!activate) {
                continue;
            }

            name_ = ReadDecoderName(activate.Get());
            Microsoft::WRL::ComPtr<IMFTransform> candidate;
            const HRESULT activate_hr = activate->ActivateObject(IID_PPV_ARGS(&candidate));
            if (FAILED(activate_hr) || !candidate) {
                last_error = name_ + " ActivateObject failed: " + HResultString(activate_hr);
                continue;
            }

            transform_ = candidate;
            if (ConfigureTransform(last_error)) {
                CoTaskMemFree(activates);
                return true;
            }
            transform_.Reset();
        }

        CoTaskMemFree(activates);
        error = last_error.empty() ? "HW H.264 decoder activation failed" : last_error;
        return false;
    }

    bool ConfigureTransform(std::string& error)
    {
        UnlockAsyncTransform();

        // Low-latency: emit each frame as soon as it's decoded instead of buffering several for
        // reordering / DXVA pipelining. The sender produces no B-frames, so no reorder is needed;
        // without this the Microsoft H264 decoder MFT adds multiple frames (~100+ ms) of latency.
        {
            Microsoft::WRL::ComPtr<IMFAttributes> attributes;
            if (SUCCEEDED(transform_->GetAttributes(&attributes)) && attributes) {
                (void)attributes->SetUINT32(MF_LOW_LATENCY, TRUE);
            }
            Microsoft::WRL::ComPtr<ICodecAPI> codec_api;
            if (SUCCEEDED(transform_.As(&codec_api)) && codec_api) {
                VARIANT low_latency;
                VariantInit(&low_latency);
                low_latency.vt = VT_BOOL;
                low_latency.boolVal = VARIANT_TRUE;
                (void)codec_api->SetValue(&CODECAPI_AVLowLatencyMode, &low_latency);
                VariantClear(&low_latency);
            }
        }

        // Bind the decoder to the caller's D3D11 device for D3D11-mode (texture) output.
        HRESULT hr = transform_->ProcessMessage(
            MFT_MESSAGE_SET_D3D_MANAGER, reinterpret_cast<ULONG_PTR>(device_manager_.Get()));
        if (FAILED(hr)) {
            error = FriendlyNameForDiagnostics() + " SET_D3D_MANAGER failed: " + HResultString(hr);
            return false;
        }

        Microsoft::WRL::ComPtr<IMFMediaType> input_type;
        hr = MFCreateMediaType(&input_type);
        if (FAILED(hr)) {
            error = "MFCreateMediaType(H264) failed: " + HResultString(hr);
            return false;
        }
        (void)input_type->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video);
        (void)input_type->SetGUID(MF_MT_SUBTYPE, InputSubtype());
        (void)MFSetAttributeSize(input_type.Get(), MF_MT_FRAME_SIZE,
            static_cast<UINT32>(width_), static_cast<UINT32>(height_));
        (void)input_type->SetUINT32(MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive);
        hr = transform_->SetInputType(0, input_type.Get(), 0);
        if (FAILED(hr)) {
            error = FriendlyNameForDiagnostics() + " SetInputType(H264) failed: " + HResultString(hr);
            return false;
        }

        // NV12 output may not be offered until the decoder parses an SPS; that's fine —
        // MF_E_TRANSFORM_STREAM_CHANGE from the first ProcessOutput triggers (re)negotiation.
        std::string ignored;
        (void)ConfigureOutputNv12(ignored);
        (void)transform_->GetOutputStreamInfo(0, &output_info_);

        (void)transform_->ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
        (void)transform_->ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        last_error_.clear();
        return true;
    }

    bool ConfigureOutputNv12(std::string& error)
    {
        for (DWORD i = 0;; ++i) {
            Microsoft::WRL::ComPtr<IMFMediaType> type;
            const HRESULT hr = transform_->GetOutputAvailableType(0, i, &type);
            if (hr == MF_E_NO_MORE_TYPES) {
                break;
            }
            if (FAILED(hr) || !type) {
                error = "GetOutputAvailableType failed: " + HResultString(hr);
                return false;
            }
            GUID subtype = GUID_NULL;
            if (SUCCEEDED(type->GetGUID(MF_MT_SUBTYPE, &subtype)) && subtype == MFVideoFormat_NV12) {
                const HRESULT set_hr = transform_->SetOutputType(0, type.Get(), 0);
                if (SUCCEEDED(set_hr)) {
                    (void)transform_->GetOutputStreamInfo(0, &output_info_);
                    return true;
                }
            }
        }
        error = "decoder offers no NV12 output type yet";
        return false;
    }

    OmniWebRtcMediaStatus DrainOneOutput(
        Microsoft::WRL::ComPtr<ID3D11Texture2D>& out_nv12,
        UINT& out_subresource,
        bool& produced)
    {
        produced = false;
        if (async_ && !WaitForAsyncEvent(METransformHaveOutput, std::chrono::milliseconds(200))) {
            return OMNI_WEBRTC_MEDIA_STATUS_OK;  // no output ready — caller should feed more input
        }

        MFT_OUTPUT_DATA_BUFFER output_data{};
        output_data.dwStreamID = 0;
        DWORD process_status = 0;
        HRESULT hr = transform_->ProcessOutput(0, 1, &output_data, &process_status);
        if (output_data.pEvents != nullptr) {
            output_data.pEvents->Release();
            output_data.pEvents = nullptr;
        }

        if (hr == MF_E_TRANSFORM_NEED_MORE_INPUT) {
            return OMNI_WEBRTC_MEDIA_STATUS_OK;
        }
        if (hr == MF_E_TRANSFORM_STREAM_CHANGE) {
            // Decoder learned the real output format — (re)negotiate NV12; no frame this call.
            std::string err;
            if (!ConfigureOutputNv12(err)) {
                last_error_ = "decoder STREAM_CHANGE renegotiation failed: " + err;
                return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
            }
            return OMNI_WEBRTC_MEDIA_STATUS_OK;
        }
        if (FAILED(hr) || output_data.pSample == nullptr) {
            last_error_ = "decoder ProcessOutput failed: " + HResultString(hr);
            if (output_data.pSample != nullptr) {
                output_data.pSample->Release();
            }
            ++decode_failures_;
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }

        Microsoft::WRL::ComPtr<IMFSample> sample;
        sample.Attach(output_data.pSample);
        output_data.pSample = nullptr;

        Microsoft::WRL::ComPtr<IMFMediaBuffer> buffer;
        hr = sample->GetBufferByIndex(0, &buffer);
        if (FAILED(hr) || !buffer) {
            last_error_ = "decoder output GetBufferByIndex failed: " + HResultString(hr);
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }
        Microsoft::WRL::ComPtr<IMFDXGIBuffer> dxgi_buffer;
        hr = buffer.As(&dxgi_buffer);
        if (FAILED(hr) || !dxgi_buffer) {
            last_error_ = "decoder output is not a D3D11 (IMFDXGIBuffer) buffer: " + HResultString(hr);
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }
        hr = dxgi_buffer->GetResource(IID_PPV_ARGS(out_nv12.ReleaseAndGetAddressOf()));
        if (FAILED(hr) || !out_nv12) {
            last_error_ = "decoder output GetResource(ID3D11Texture2D) failed: " + HResultString(hr);
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }
        (void)dxgi_buffer->GetSubresourceIndex(&out_subresource);
        produced = true;
        ++frames_decoded_;
        return OMNI_WEBRTC_MEDIA_STATUS_OK;
    }

    bool WrapAnnexBSample(
        const uint8_t* annexb,
        size_t len,
        int64_t timestamp_100ns,
        Microsoft::WRL::ComPtr<IMFSample>& out_sample)
    {
        Microsoft::WRL::ComPtr<IMFMediaBuffer> buffer;
        HRESULT hr = MFCreateMemoryBuffer(static_cast<DWORD>(len), &buffer);
        if (FAILED(hr)) {
            last_error_ = "MFCreateMemoryBuffer(input) failed: " + HResultString(hr);
            return false;
        }
        BYTE* target = nullptr;
        DWORD max_len = 0;
        DWORD current_len = 0;
        hr = buffer->Lock(&target, &max_len, &current_len);
        if (FAILED(hr) || target == nullptr || max_len < len) {
            last_error_ = "input buffer Lock failed: " + HResultString(hr);
            if (SUCCEEDED(hr)) {
                (void)buffer->Unlock();
            }
            return false;
        }
        std::memcpy(target, annexb, len);
        (void)buffer->Unlock();
        (void)buffer->SetCurrentLength(static_cast<DWORD>(len));

        hr = MFCreateSample(out_sample.ReleaseAndGetAddressOf());
        if (FAILED(hr)) {
            last_error_ = "MFCreateSample(input) failed: " + HResultString(hr);
            return false;
        }
        (void)out_sample->AddBuffer(buffer.Get());
        (void)out_sample->SetSampleTime(static_cast<LONGLONG>(timestamp_100ns));
        return true;
    }

    void UnlockAsyncTransform()
    {
        Microsoft::WRL::ComPtr<IMFAttributes> attributes;
        if (FAILED(transform_->GetAttributes(&attributes)) || !attributes) {
            return;
        }
        UINT32 async = 0;
        if (SUCCEEDED(attributes->GetUINT32(MF_TRANSFORM_ASYNC, &async)) && async != 0) {
            (void)attributes->SetUINT32(MF_TRANSFORM_ASYNC_UNLOCK, TRUE);
            async_ = true;
            (void)transform_.As(&event_generator_);
        }
    }

    bool WaitForAsyncEvent(MediaEventType expected, std::chrono::milliseconds timeout)
    {
        if (!event_generator_) {
            return false;
        }
        const auto deadline = std::chrono::steady_clock::now() + timeout;
        while (std::chrono::steady_clock::now() < deadline) {
            Microsoft::WRL::ComPtr<IMFMediaEvent> event;
            const HRESULT hr = event_generator_->GetEvent(MF_EVENT_FLAG_NO_WAIT, &event);
            if (hr == MF_E_NO_EVENTS_AVAILABLE) {
                std::this_thread::sleep_for(std::chrono::milliseconds(2));
                continue;
            }
            if (FAILED(hr) || !event) {
                return false;
            }
            MediaEventType type = MEUnknown;
            if (SUCCEEDED(event->GetType(&type)) && type == expected) {
                return true;
            }
        }
        return false;
    }

    static std::string ReadDecoderName(IMFActivate* activate)
    {
        wchar_t* friendly_name = nullptr;
        UINT32 friendly_name_length = 0;
        if (activate != nullptr &&
            SUCCEEDED(activate->GetAllocatedString(
                MFT_FRIENDLY_NAME_Attribute, &friendly_name, &friendly_name_length)) &&
            friendly_name != nullptr && friendly_name_length > 0) {
            std::string name = WideToUtf8(friendly_name);
            CoTaskMemFree(friendly_name);
            return name;
        }
        if (friendly_name != nullptr) {
            CoTaskMemFree(friendly_name);
        }
        return "Media Foundation hardware H.264 decoder";
    }

    std::string FriendlyNameForDiagnostics() const
    {
        return name_.empty() ? "Media Foundation hardware H.264 decoder" : name_;
    }

    Microsoft::WRL::ComPtr<IMFTransform> transform_;
    Microsoft::WRL::ComPtr<IMFMediaEventGenerator> event_generator_;
    Microsoft::WRL::ComPtr<IMFDXGIDeviceManager> device_manager_;
    MFT_OUTPUT_STREAM_INFO output_info_{};
    UINT reset_token_ = 0;
    int width_ = 0;
    int height_ = 0;
    bool async_ = false;
    bool co_initialized_ = false;
    DWORD co_init_thread_ = 0;
    bool mf_started_ = false;
    uint64_t frames_decoded_ = 0;
    uint64_t decode_failures_ = 0;
    std::string name_;
    std::string last_error_;
    NativeVideoCodec codec_ = NativeVideoCodec::H264;

    GUID InputSubtype() const
    {
        switch (codec_) {
        case NativeVideoCodec::H265:
            return MFVideoFormat_HEVC;
        case NativeVideoCodec::Av1:
            return MFVideoFormat_AV1;
        default:
            return MFVideoFormat_H264;
        }
    }
};
#endif

// Wave 5 Phase 5a (2026-05-16): adapter satisfying webrtc::PacingController's
// PacketSender contract. Defined out-of-line after LibWebRtcVideoSession so
// SendPacket can forward into the session's outgoing transport queue.
// The class itself is forward-declared here; full definition appears below.
//
// FetchFec / GeneratePadding возвращают пустоту. Это РАЗНЫЕ причины, и путать их не надо: FEC у
// нас нет как избыточности (#23 показал, что она не обоснована), а добивка отсутствует потому, что
// без RTX она вредна — подробности и замер у самого метода GeneratePadding.
// SSRC потока Omni. Константа на процесс, и это не удобство: с точки зрения собеседника каждая
// созданная здесь сессия — ОДИН И ТОТ ЖЕ поток, а у потока номера пакетов не идут назад.
//
// Рядом жил свой счётчик номеров g_next_video_rtp_sequence. Он был вынесен на процесс ровно по
// этой причине: пока он был полем сессии и начинался с 1, каждая пересборка кодировщика — камера
// → экран, смена разрешения, шаг ABR — начинала нумерацию заново под неизменившимся SSRC, и
// приёмник выбрасывал «старые» пакеты. Ошибка пряталась, пока оболочка пересобирала декодер
// вместе с кодировщиком; как только декодер стал долгоживущим, она вышла наружу: камера идёт,
// экран после неё мёртв (docs/bugs/screen-share-had-no-frame-rate.md).
//
// Счётчика больше нет: номера ставит PacketSequencer внутри модуля, а непрерывность между
// сессиями держит g_omni_video_rtp_state ниже — тем же способом, что и апстрим.
/// Окружение сессии. ЕДИНСТВЕННОЕ место, где оно строится, — чтобы подмена часов была тоже
/// единственной, а не расползлась по четырём вызовам CreateEnvironment.
/// Опыты, включённые для ВСЕХ сессий.
///
/// RFC8888CongestionControlFeedback — не украшение, а условие работы приёмной части оценки канала:
/// RTCPReceiver разбирает отчёт о доставке ТОЛЬКО при включённом опыте
/// (rtcp_receiver.cc:160,421), а иначе молча считает пакет пропущенным. Ошибка ровно того рода,
/// которую этот файл ловит у себя весь разбор: всё собирается, всё шлётся, отчёты доезжают — и не
/// делают ничего.
///
/// SendNackDelayMs/10 — запросчик NACK ждёт 10 мс, прежде чем просить повтор дыры. Без задержки
/// он просит в ту же секунду, как заметил дыру, а на реальном пути ~17 % видео-пакетов приходят
/// не по порядку — и почти все в пределах 2 мс (замер ноутбук→ПК: 1670 из 1674 переставленных
/// опоздали ≤2 мс, при нуле потерь). Каждый такой пакет давал ложную просьбу и целый лишний повтор:
/// 1674 просьбы и 1611 повторов за 45 с, ~17 % трафика впустую, а в группе повтор уходит всем.
/// С задержкой 10 мс — 1 просьба и 1 повтор при той же перестановке. Своя статистика перестановок
/// у запросчика выключена в апстриме (is_retransmitted всегда true), поэтому ручка — эта.
/// Цена — 10 мс к восстановлению настоящей потери. Разбор:
/// docs/bugs/group-call-joiner-saw-no-video.md, «Ложные просьбы о повторе».
constexpr char kOmniFieldTrials[] =
    "WebRTC-RFC8888CongestionControlFeedback/Enabled/"
    "WebRTC-SendNackDelayMs/10/";

/// Выполнить задачу НА очереди и ДОЖДАТЬСЯ её.
///
/// «TaskQueueDeleter дренирует очередь и блокирует до конца задач» — так было написано в этом
/// файле, и это НЕВЕРНО. TaskQueueWin::Delete шлёт потоку WM_QUIT: цикл сообщений выходит, а
/// поставленные в него задачи просто пропадают. Пока посланная при разрушении задача только
/// обнуляла указатели, потеря была безобидной — члены всё равно разрушались следом, пусть и на
/// чужом потоке. Перестало быть безобидным на перевороте #57: ~RtpTransportControllerSend снимает
/// поток с пейсера, а тот шлёт задачу на свою очередь. Задача пропала -> сборщик умер на потоке
/// разрушителя, уже после очереди, и попал в PostTask мёртвой очереди.
///
/// Снято 2026-08-25, три падения из пяти прогонов:
///   ACCESS_VIOLATION, write to 0x24
///   #03 TaskQueueWin ... #04 TaskQueuePacedSender::RemovePacketsForSsrc
///   #05 RtpTransportControllerSend::DeRegisterSendingRtpStream
///   #06 ~RtpVideoSender  #08 ~RtpTransportControllerSend  #09 ~LibWebRtcVideoSession
///
/// Под виртуальными часами очередь вообще ничего не выполняет сама — там задачу двигает
/// RunPendingSimulatedTasks. Поэтому здесь оба случая: сперва подвинуть виртуальное время, потом
/// дождаться.
template <typename Fn>
void RunOnQueueAndWait(webrtc::TaskQueueBase* queue, Fn&& fn);

/// Дать выполниться задачам, уже стоящим в очередях, под ВИРТУАЛЬНЫМИ часами.
///
/// Под реальными часами очередь крутит поток, и её деструктор дожидается конца задач сам. Под
/// виртуальными задачи не выполняются НИКОГДА, пока кто-нибудь не подвинет время: посланная задача
/// просто пропадает вместе с очередью.
///
/// До #57 это было безобидно — все посланные при разрушении задачи только обнуляли указатели.
/// Перестало быть безобидным, когда среди них появилось разрушение RtpTransportControllerSend: он
/// снимает поток с пейсера, а тот шлёт задачу на свою очередь. Пропала задача разрушения — объект
/// умер на ЧУЖОМ потоке, уже после очереди, и падение получилось прямо в PostTaskImpl мёртвой
/// очереди (ACCESS_VIOLATION, стек снят 2026-08-25).
void RunPendingSimulatedTasks()
{
    webrtc::GlobalSimulatedTimeController* controller = nullptr;
    {
        std::lock_guard<std::mutex> lock(g_simulated_time_mutex);
        controller = g_simulated_time.get();
    }
    if (controller != nullptr) {
        // Замок отпущен намеренно — та же причина, что и в advance_ms: внутри выполняются задачи,
        // а они могут снова спросить окружение.
        controller->AdvanceTime(webrtc::TimeDelta::Zero());
    }
}

template <typename Fn>
void RunOnQueueAndWait(webrtc::TaskQueueBase* queue, Fn&& fn)
{
    if (queue == nullptr) {
        return;
    }
    std::promise<void> done;
    std::future<void> waiter = done.get_future();
    queue->PostTask([&done, task = std::forward<Fn>(fn)]() mutable {
        task();
        done.set_value();
    });
    RunPendingSimulatedTasks();
    waiter.wait();
}

/// ЖУРНАЛ РЕШЕНИЙ РЕГУЛЯТОРА — ШТАТНЫЙ ПРИБОР LIBWEBRTC, А НЕ НАШ.
///
/// GoogCC сам сообщает в RtcEventLog каждое своё решение: оценку по задержке и состояние детектора,
/// оценку по потерям с долей потерь, запуск пробы и её исход с причиной отказа. Этим журналом
/// разработчики libwebrtc и разбирают «почему цель такая». Без него видна только итоговая цель, и
/// вопрос «что её держит — задержка, потери или неудачные пробы» не решается (2026-09-24: на UDP
/// цель ползёт 1–2 Мбит/с, на TCP-двери та же пара сразу берёт 4). Файл не пишется — здесь только
/// последние значения и счётчики для диагностики сессии. Потокобезопасен: пишут очереди регулятора.
class OmniBweEventLog final : public webrtc::RtcEventLog {
public:
    struct Snapshot {
        int32_t delay_bps = 0;
        int delay_state = 0;  // BandwidthUsage: 0 normal, 1 underusing, 2 overusing
        uint64_t delay_updates = 0;
        uint64_t delay_overuse_updates = 0;
        int32_t loss_bps = 0;
        uint8_t loss_fraction = 0;
        uint64_t loss_updates = 0;
        uint64_t probes_created = 0;
        uint64_t probes_succeeded = 0;
        int32_t probe_last_success_bps = 0;
        uint64_t probes_failed = 0;
        int probe_last_failure_reason = -1;
    };

    bool StartLogging(std::unique_ptr<webrtc::RtcEventLogOutput>, int64_t) override { return false; }
    void StopLogging() override {}

    void Log(std::unique_ptr<webrtc::RtcEvent> event) override
    {
        if (!event) {
            return;
        }
        std::lock_guard<std::mutex> lock(mutex_);
        switch (event->GetType()) {
        case webrtc::RtcEvent::Type::BweUpdateDelayBased: {
            const auto& e = static_cast<const webrtc::RtcEventBweUpdateDelayBased&>(*event);
            snapshot_.delay_bps = e.bitrate_bps();
            snapshot_.delay_state = static_cast<int>(e.detector_state());
            ++snapshot_.delay_updates;
            if (e.detector_state() == webrtc::BandwidthUsage::kBwOverusing) {
                ++snapshot_.delay_overuse_updates;
            }
            break;
        }
        case webrtc::RtcEvent::Type::BweUpdateLossBased: {
            const auto& e = static_cast<const webrtc::RtcEventBweUpdateLossBased&>(*event);
            snapshot_.loss_bps = e.bitrate_bps();
            snapshot_.loss_fraction = e.fraction_loss();
            ++snapshot_.loss_updates;
            break;
        }
        case webrtc::RtcEvent::Type::ProbeClusterCreated:
            ++snapshot_.probes_created;
            break;
        case webrtc::RtcEvent::Type::ProbeResultSuccess: {
            const auto& e = static_cast<const webrtc::RtcEventProbeResultSuccess&>(*event);
            ++snapshot_.probes_succeeded;
            snapshot_.probe_last_success_bps = e.bitrate_bps();
            break;
        }
        case webrtc::RtcEvent::Type::ProbeResultFailure: {
            const auto& e = static_cast<const webrtc::RtcEventProbeResultFailure&>(*event);
            ++snapshot_.probes_failed;
            snapshot_.probe_last_failure_reason = static_cast<int>(e.failure_reason());
            break;
        }
        default:
            break;
        }
    }

    Snapshot snapshot() const
    {
        std::lock_guard<std::mutex> lock(mutex_);
        return snapshot_;
    }

private:
    mutable std::mutex mutex_;
    Snapshot snapshot_;
};

webrtc::Environment MakeOmniEnvironment(webrtc::RtcEventLog* event_log = nullptr)
{
    std::lock_guard<std::mutex> lock(g_simulated_time_mutex);
    webrtc::EnvironmentFactory factory;
    factory.Set(std::make_unique<webrtc::FieldTrials>(kOmniFieldTrials));
    if (event_log != nullptr) {
        factory.Set(event_log);
    }
    if (g_simulated_time) {
        factory.Set(g_simulated_time->GetClock());
        factory.Set(g_simulated_time->GetTaskQueueFactory());
    }
    return factory.Create();
}

#if defined(_WIN32)
/// ПРИЁМ ЗВУКА — NetEq ИЗ libwebrtc (2026-09-27).
///
/// Здесь был самодельный приёмник: декодировал каждый пакет в момент прихода и дописывал звук в
/// очередь устройства. Буфера джиттера у него не было, поэтому (замер без устройств, сценарный
/// смоук): пакет, пришедший позже соседа, проигрывался второй раз после него — +1880 мс лишнего
/// звука за 10 с при 20 % перестановок, очередь упиралась в предохранитель 1 с и срезалась; а один
/// затор на 300 мс навсегда добавлял 310 мс задержки — сжимать очередь было нечем.
/// См. docs/bugs/audio-latency-only-grew.md.
///
/// NetEq делает то, что делает приём звука у апстрима: переупорядочивает, держит адаптивный буфер
/// джиттера, растягивает и сжимает время (задержка после затора возвращается к цели), маскирует
/// потери и сам достаёт избыточность Opus. Звук из него ВЫТЯГИВАЮТ по 10 мс — устройство или, у
/// зонда уровня без устройства, догоняющий опрос.
///
/// Потокобезопасности у NetEq нет («thread-compatible»): вставка идёт с потока ядра, вытягивание — с
/// потока устройства, поэтому один замок на оба.
class OmniNetEqAudioReceiver {
public:
    static constexpr int kSampleRate = 48000;
    static constexpr size_t kPullSamples = 480;  // 10 ms, mono

    /// `clock` — только для стендов на виртуальном времени; в продукте часы окружения.
    static std::unique_ptr<OmniNetEqAudioReceiver> Create(std::string* error, webrtc::Clock* clock = nullptr)
    {
        auto receiver = std::unique_ptr<OmniNetEqAudioReceiver>(new OmniNetEqAudioReceiver(
            clock == nullptr ? MakeOmniEnvironment() : MakeClockedEnvironment(clock)));
        webrtc::NetEq::Config config;
        config.sample_rate_hz = kSampleRate;
        receiver->neteq_ = webrtc::DefaultNetEqFactory().Create(
            receiver->env_, config, webrtc::CreateOpusAudioDecoderFactory());
        // Opus в SDP всегда «/2»; моно ли поток, решает параметр stereo, которого мы не ставим.
        if (!receiver->neteq_ ||
            !receiver->neteq_->RegisterPayloadType(
                OmniAudioSender::kDefaultPayloadType, webrtc::SdpAudioFormat("opus", kSampleRate, 2))) {
            if (error != nullptr) {
                *error = "NetEq could not be created for Opus";
            }
            return nullptr;
        }
        return receiver;
    }

    enum class InsertResult { kAccepted, kNotAudio, kRejected };

    /// Любой входящий пакет звонка: не наш звук (видео, RTCP) отбрасывается молча.
    InsertResult InsertRtpPacket(const uint8_t* packet, size_t packet_len)
    {
        webrtc::RtpPacketReceived parsed;
        if (packet == nullptr || !parsed.Parse(packet, packet_len) ||
            parsed.PayloadType() != OmniAudioSender::kDefaultPayloadType ||
            parsed.Ssrc() != OmniAudioSender::kAudioSsrc || parsed.payload_size() == 0) {
            return InsertResult::kNotAudio;
        }
        webrtc::RTPHeader header;
        parsed.GetHeader(&header);
        std::lock_guard<std::mutex> lock(mutex_);
        const int status = neteq_->InsertPacket(
            header,
            std::span<const uint8_t>(parsed.payload().data(), parsed.payload_size()),
            env_.clock().CurrentTime());
        return status == webrtc::NetEq::kOK ? InsertResult::kAccepted : InsertResult::kRejected;
    }

    /// Ровно 10 мс моно 48 кГц в `out`; при ошибке — тишина.
    void Pull10Ms(int16_t* out)
    {
        std::lock_guard<std::mutex> lock(mutex_);
        bool muted = false;
        const int status = neteq_->GetAudio(&frame_, &muted);
        if (status != webrtc::NetEq::kOK || muted ||
            frame_.samples_per_channel_ != kPullSamples || frame_.num_channels_ == 0) {
            std::fill(out, out + kPullSamples, int16_t{0});
            return;
        }
        const int16_t* data = frame_.data();
        const size_t channels = frame_.num_channels_;
        for (size_t i = 0; i < kPullSamples; ++i) {
            int32_t sum = 0;
            for (size_t c = 0; c < channels; ++c) {
                sum += data[i * channels + c];
            }
            out[i] = static_cast<int16_t>(sum / static_cast<int32_t>(channels));
        }
    }

    /// Нижняя граница задержки — для липсинка с видео (как у апстрима AudioReceiveStream).
    void SetMinimumDelayMs(int delay_ms)
    {
        std::lock_guard<std::mutex> lock(mutex_);
        (void)neteq_->SetMinimumDelay(delay_ms);
    }

    webrtc::NetEqNetworkStatistics CurrentStatistics() const
    {
        std::lock_guard<std::mutex> lock(mutex_);
        return neteq_->CurrentNetworkStatistics();
    }

    webrtc::NetEqLifetimeStatistics LifetimeStatistics() const
    {
        std::lock_guard<std::mutex> lock(mutex_);
        return neteq_->GetLifetimeStatistics();
    }

private:
    explicit OmniNetEqAudioReceiver(webrtc::Environment env) : env_(std::move(env)) {}

    static webrtc::Environment MakeClockedEnvironment(webrtc::Clock* clock)
    {
        webrtc::EnvironmentFactory factory;
        factory.Set(std::make_unique<webrtc::FieldTrials>(kOmniFieldTrials));
        factory.Set(clock);
        return factory.Create();
    }

    webrtc::Environment env_;
    mutable std::mutex mutex_;
    std::unique_ptr<webrtc::NetEq> neteq_;
    webrtc::AudioFrame frame_;
};
#endif

constexpr uint32_t kOmniVideoSsrc = 0x4F4D4E49u;  // "OMNI"

/// ПОТОК RTX: свой SSRC и свой номерной ряд.
///
/// Зачем он вообще нужен. Повтор и добивка обязаны иметь СОБСТВЕННУЮ нумерацию, иначе они занимают
/// номера в ряду видео, и приёмник не может отличить «пакет потерялся» от «этого номера у видео
/// никогда и не было». Замер 2026-08-22 показал это на добивке в чистом виде: доставка упала с 70
/// кадров до 2, переспросов стало 317 вместо 143, из кэша вытеснено 130 настоящих видеопакетов.
/// Именно поэтому апстрим шлёт и повторы, и добивку отдельным потоком, и порядок работ у нас
/// записан как «сперва RTX, потом добивка».
///
/// Значения фиксированы, а не согласуются, — по тем же основаниям и с той же оговоркой, что у
/// kGenericFrameDescriptorExtensionId и профиля H.264: оба конца это наша сборка. Соседний SSRC и
/// тип нагрузки «медиа + 1» — соглашение апстрима, не выдумка.
constexpr uint32_t kOmniVideoRtxSsrc = kOmniVideoSsrc + 1u;

/// SSRC, которым ПРИЁМНИК подписывает свой RTCP.
///
/// Он обязан отличаться от медийного: в отчёте приёмника поле «отправитель» — это он сам, а блоки
/// внутри рассказывают о ЧУЖОМ потоке. Совпади они, и приёмник отчитывался бы о себе.
constexpr uint32_t kOmniVideoReceiverSsrc = kOmniVideoSsrc + 2u;

constexpr uint8_t kOmniVideoRtxPayloadType = 103;

/// Глубина истории отправки, из которой берутся повторы. Значение апстрима
/// (`rtp_video_sender.cc:91`), не подобранное: при 30 к/с и пяти пакетах на кадр это около
/// четырёх секунд — заведомо больше любого разумного времени на переспрос.

/// Та же глубина, выраженная временем — так её просит RtpConfig::nack.rtp_history_ms. 600 пакетов
/// при пяти на кадр и 30 к/с это около четырёх секунд; берём округлённо, как апстрим.
constexpr int kSendSideHistoryMs = 4000;

// СОСТОЯНИЕ RTP ДЛЯ АПСТРИМОВОГО МОДУЛЯ — ТОЖЕ НА ПРОЦЕСС, И ПО ТОЙ ЖЕ ПРИЧИНЕ.
//
// Счётчик выше сделан process-wide не из удобства: SSRC у нас константа, поэтому с точки зрения
// собеседника каждая созданная в этом процессе сессия — ОДИН И ТОТ ЖЕ поток, а у потока номера не
// имеют права идти назад. Когда счётчик был полем сессии и начинался с единицы, каждая пересборка
// кодировщика (камера -> демонстрация экрана, смена разрешения) перезапускала нумерацию, и приёмник
// молча отбрасывал «старые» пакеты (docs/bugs/screen-share-had-no-frame-rate.md).
//
// RTPSender внутри ModuleRtpRtcpImpl2 начинает нумерацию со СВОЕГО значения, то есть наступил бы на
// те же грабли. У апстрима на это есть штатный механизм — SetRtpState/GetRtpState, — и мы берём
// именно его, а не приделываем свой счётчик к чужому отправителю: состояние снимается при
// разрушении модуля и восстанавливается в следующий.
std::mutex g_omni_video_rtp_state_mutex;
bool g_omni_video_rtp_state_valid = false;
webrtc::RtpState g_omni_video_rtp_state;

class LibWebRtcVideoSession;

// НАБЛЮДАТЕЛЬ ОЦЕНКИ КАНАЛА ОТ СБОРЩИКА (#57, шаг C2b-1).
//
// RtpTransportControllerSend отдаёт цель через него — это его штатный выход, тот же, которым
// пользуется VideoSendStream. Пока только запоминаем: ABR ещё живёт на нашей оценке, и переключать
// его вслепую нельзя. Сперва надо увидеть, что сборщик ВООБЩЕ что-то посчитал, и только потом на
// это опираться.
class OmniTargetRateObserver final : public webrtc::TargetTransferRateObserver {
public:
    void OnTargetTransferRate(webrtc::TargetTransferRate rate) override
    {
        target_bps_.store(rate.target_rate.bps(), std::memory_order_relaxed);
        updates_.fetch_add(1, std::memory_order_relaxed);
        // ЦЕЛЬ ОБЯЗАНА ДОЙТИ ДО КОДИРОВЩИКА, А НЕ ТОЛЬКО ДО ДИАГНОСТИКИ (#61, шаг D2b).
        //
        // VideoStreamEncoder начинает с нулевой цели и до первого OnBitrateUpdated НЕ КОДИРУЕТ
        // ВООБЩЕ — просто выбрасывает кадры. Первый прогон после переворота это и показал:
        // «rampup: nothing was delivered», 0 отправленных байт при живом источнике кадров.
        if (on_target_) {
            on_target_(rate);
        }
    }

    /// Куда отдавать цель дальше. Ставится один раз, до первого отчёта.
    void SetTargetSink(std::function<void(webrtc::TargetTransferRate)> sink)
    {
        on_target_ = std::move(sink);
    }

    int64_t target_bps() const { return target_bps_.load(std::memory_order_relaxed); }
    uint64_t updates() const { return updates_.load(std::memory_order_relaxed); }

private:
    std::atomic<int64_t> target_bps_{0};
    std::atomic<uint64_t> updates_{0};
    std::function<void(webrtc::TargetTransferRate)> on_target_;
};

// СОБРАННЫЙ КАДР ПЛЮС ТО, ЧЕГО В НЁМ НЕ ХВАТАЕТ БУФЕРУ ПОКАЗА.
//
// Три вещи, и каждая — не украшение.
//
// 1. ВРЕМЯ ПРИБЫТИЯ. RtpVideoFrameAssembler его НЕ ПРОСТАВЛЯЕТ: в конструктор RtpFrameObject уходит
//    Timestamp::Zero(), а рядом висит TODO самого апстрима (bugs.webrtc.org/42222730). Пока кадры
//    разбирала наша обвязка, это было безразлично — она время прибытия хранила отдельно. Но
//    VideoStreamBufferController строит по нему экстраполятор меток и время показа: ноль означал бы
//    «кадр пришёл в эпоху», FrameHasBadRenderTiming срабатывал бы на каждом кадре, и оценка
//    джиттера вместе с модулем времени сбрасывалась бы столько же раз.
//
// 2. ЗАДЕРЖАН ЛИ КАДР ПОВТОРОМ. Сборщик всегда ставит times_nacked = 0, поэтому у него ответ
//    всегда «нет». Для буфера показа это не мелочь: по этому признаку он решает, кормить ли оценку
//    джиттера межкадровым интервалом. Интервал кадра, достроенного из RTX, — это не джиттер сети, а
//    время круга повтора, и принять одно за другое значит раздуть задержку показа на ровном месте.
//    Мы знаем правду: восстановленные пакеты помечены recovered() ещё на входе.
//
// 3. ЗАГОЛОВОК И НОМЕРА ПАКЕТОВ. Декодер читает их приведением к RtpFrameObject. Так делать было
//    нельзя и раньше: при склейке многослойной единицы буфер показа отдаёт CombineAndDeleteFrames,
//    то есть ПЛОСКИЙ EncodedFrame, и приведение стало бы неопределённым поведением. У нас слой один,
//    поэтому не стреляло. Теперь приведение всегда к СВОЕМУ типу, который мы сами и создали.
class OmniAssembledFrame final : public webrtc::EncodedFrame {
public:
    OmniAssembledFrame(
        const webrtc::RtpFrameObject& source,
        webrtc::Timestamp received_at,
        bool delayed_by_retransmission)
        : webrtc::EncodedFrame(source),
          video_header_(source.GetRtpVideoHeader()),
          received_at_(received_at),
          first_seq_num_(source.first_seq_num()),
          last_seq_num_(source.last_seq_num()),
          delayed_by_retransmission_(delayed_by_retransmission)
    {
        // СВЕДЕНИЯ О ПАКЕТАХ — РАДИ ОДНОГО ЧИСЛА, НО НАСТОЯЩЕГО.
        //
        // Буфер показа считает «сколько кадр пролежал в буфере» как now - MinReceiveTime, а
        // MinReceiveTime берётся из PacketInfos. Сборщик их не заполняет вовсе, поэтому
        // величина выходила бесконечной и обрезалась в ноль — то есть диагностика показывала
        // ноль ВСЕГДА и ни о чём не говорила. Одной записи достаточно: раньше времени прибытия
        // первого пакета кадр всё равно не соберётся.
        webrtc::RtpPacketInfo info(
            kOmniVideoSsrc, /*csrcs=*/{}, source.RtpTimestamp(), received_at);
        SetPacketInfos(webrtc::RtpPacketInfos({info}));
    }

    std::optional<webrtc::Timestamp> ReceivedTimestamp() const override { return received_at_; }
    int64_t ReceivedTime() const override { return received_at_.ms(); }
    bool delayed_by_retransmission() const override { return delayed_by_retransmission_; }

    const webrtc::RTPVideoHeader& GetRtpVideoHeader() const { return video_header_; }
    uint16_t first_seq_num() const { return first_seq_num_; }
    uint16_t last_seq_num() const { return last_seq_num_; }

private:
    webrtc::RTPVideoHeader video_header_;
    webrtc::Timestamp received_at_;
    uint16_t first_seq_num_;
    uint16_t last_seq_num_;
    bool delayed_by_retransmission_;
};

// СТАТИСТИКА БУФЕРА ПОКАЗА — ОБЯЗАТЕЛЬНЫЙ СОБЕСЕДНИК КОНТРОЛЛЕРА.
//
// VideoStreamBufferController не хранит счётчиков: всё, что он знает о себе, он отдаёт сюда.
// Значения кладутся атомарно, потому что читает их диагностика с чужого потока.
class OmniPlayoutStatsObserver final : public webrtc::VideoStreamBufferControllerStatsObserver {
public:
    void OnCompleteFrame(bool is_keyframe, size_t size_bytes, webrtc::VideoContentType) override
    {
        (void)size_bytes;
        // СЧИТАЕТСЯ НЕ «КАДР», А «ВСТАВКА, ПРОДВИНУВШАЯ НЕПРЕРЫВНУЮ ЦЕПОЧКУ».
        // Контроллер зовёт это один раз на вставку, даже если та сделала непрерывными сразу
        // несколько кадров (video_stream_buffer_controller.cc:167). Под потерями и
        // переупорядочиванием число выходит МЕНЬШЕ числа кадров, и называть его «полными
        // кадрами» значило бы завести ещё одно имя, которое врёт.
        continuity_advances_.fetch_add(1, std::memory_order_relaxed);
        if (is_keyframe) {
            keyframes_.fetch_add(1, std::memory_order_relaxed);
        }
    }

    void OnDroppedFrames(uint32_t frames_dropped) override
    {
        dropped_frames_.fetch_add(frames_dropped, std::memory_order_relaxed);
    }

    void OnDecodableFrame(
        webrtc::TimeDelta jitter_buffer_delay,
        webrtc::TimeDelta target_delay,
        webrtc::TimeDelta minimum_delay) override
    {
        // СРЕДНЕЕ, А НЕ ПОСЛЕДНЕЕ. Последнее значение снимается в конце прогона, когда поток уже
        // кончился и остаток буфера доигрывается, — то есть описывает хвост, а не установившийся
        // режим. Разница не теоретическая: ради неё и заведено.
        jitter_buffer_delay_sum_ms_.fetch_add(jitter_buffer_delay.ms(), std::memory_order_relaxed);
        jitter_buffer_delay_count_.fetch_add(1, std::memory_order_relaxed);
        jitter_buffer_delay_max_ms_.store(
            std::max(jitter_buffer_delay_max_ms_.load(std::memory_order_relaxed),
                     jitter_buffer_delay.ms()),
            std::memory_order_relaxed);
        target_delay_ms_.store(target_delay.ms(), std::memory_order_relaxed);
        minimum_delay_ms_.store(minimum_delay.ms(), std::memory_order_relaxed);
    }

    void OnFrameBufferTimingsUpdated(
        int estimated_max_decode_time_ms,
        int current_delay_ms,
        int /*target_delay_ms*/,
        int /*jitter_delay_ms*/,
        int /*min_playout_delay_ms*/,
        int /*render_delay_ms*/) override
    {
        estimated_decode_time_ms_.store(estimated_max_decode_time_ms, std::memory_order_relaxed);
        current_delay_ms_.store(current_delay_ms, std::memory_order_relaxed);
    }

    uint64_t continuity_advances() const { return continuity_advances_.load(std::memory_order_relaxed); }
    uint64_t keyframes() const { return keyframes_.load(std::memory_order_relaxed); }
    uint64_t dropped_frames() const { return dropped_frames_.load(std::memory_order_relaxed); }
    int64_t jitter_buffer_delay_ms() const
    {
        const uint64_t count = jitter_buffer_delay_count_.load(std::memory_order_relaxed);
        return count == 0 ? 0
                          : jitter_buffer_delay_sum_ms_.load(std::memory_order_relaxed) /
                                static_cast<int64_t>(count);
    }
    int64_t jitter_buffer_delay_max_ms() const { return jitter_buffer_delay_max_ms_.load(std::memory_order_relaxed); }
    int64_t target_delay_ms() const { return target_delay_ms_.load(std::memory_order_relaxed); }
    int64_t minimum_delay_ms() const { return minimum_delay_ms_.load(std::memory_order_relaxed); }
    int64_t estimated_decode_time_ms() const { return estimated_decode_time_ms_.load(std::memory_order_relaxed); }
    int64_t current_delay_ms() const { return current_delay_ms_.load(std::memory_order_relaxed); }

private:
    std::atomic<uint64_t> continuity_advances_{0};
    std::atomic<uint64_t> keyframes_{0};
    std::atomic<uint64_t> dropped_frames_{0};
    std::atomic<int64_t> jitter_buffer_delay_sum_ms_{0};
    std::atomic<uint64_t> jitter_buffer_delay_count_{0};
    std::atomic<int64_t> jitter_buffer_delay_max_ms_{0};
    std::atomic<int64_t> target_delay_ms_{0};
    std::atomic<int64_t> minimum_delay_ms_{0};
    std::atomic<int64_t> estimated_decode_time_ms_{0};
    std::atomic<int64_t> current_delay_ms_{0};
};

// ВЫХОД БУФЕРА ПОКАЗА: «кадр пора декодировать» и «декодировать нечего слишком долго».
//
// Роль апстримовского VideoReceiveStream2 в паре с контроллером. Обе точки зовутся на очереди
// показа.
class OmniPlayoutReceiver final : public webrtc::FrameSchedulingReceiver {
public:
    explicit OmniPlayoutReceiver(LibWebRtcVideoSession* owner) : owner_(owner) {}

    void OnEncodedFrame(std::unique_ptr<webrtc::EncodedFrame> frame) override;
    void OnDecodableFrameTimeout(webrtc::TimeDelta wait_time) override;

private:
    LibWebRtcVideoSession* owner_;
};

// ВЫХОД ОРКЕСТРАТОРА КОДИРОВАНИЯ (#61, шаг D2b).
//
// VideoStreamEncoder отдаёт закодированный кадр сюда, и отсюда он идёт прямо в
// RtpVideoSender::OnEncodedImage. Между ними больше нет ничего: ни нашей пакетизации, ни ручной
// сборки описателя структуры кадров, ни очереди с опросом. Структуру строит RtpPayloadParams из
// CodecSpecificInfo, который даёт кодировщик, — а он его теперь даёт, потому что кодировщик стал
// webrtc::VideoEncoder (шаг D1).
class OmniEncoderSink final : public webrtc::VideoStreamEncoderInterface::EncoderSink {
public:
    explicit OmniEncoderSink(LibWebRtcVideoSession* owner) : owner_(owner) {}

    webrtc::EncodedImageCallback::Result OnEncodedImage(
        const webrtc::EncodedImage& encoded_image,
        const webrtc::CodecSpecificInfo* codec_specific_info) override;

    void OnDroppedFrame(DropReason reason) override;

    void OnEncoderConfigurationChanged(
        std::vector<webrtc::VideoStream> /*streams*/,
        bool /*is_svc*/,
        webrtc::VideoEncoderConfig::ContentType /*content_type*/,
        int /*min_transmit_bitrate_bps*/) override
    {
    }

    void OnBitrateAllocationUpdated(
        const webrtc::VideoBitrateAllocation& /*allocation*/) override
    {
    }

    void OnVideoLayersAllocationUpdated(webrtc::VideoLayersAllocation /*allocation*/) override
    {
    }

private:
    LibWebRtcVideoSession* owner_;
};

// ПРИЁМНИК ВОССТАНОВЛЕННЫХ ИЗ RTX ПАКЕТОВ.
//
// webrtc::RtxReceiveStream снимает с пакета обёртку RTX — двухбайтовый заголовок с исходным
// номером — и отдаёт восстановленный медиапакет сюда (rtx_receive_stream.cc:104-116). Своей
// логики здесь нет и быть не должно: разбор RTX это RFC 4588, и писать его руками ради двух
// байт — ровно тот случай, когда самоделка обходится дороже, чем кажется.
//
// Пакеты-добивки (полезная нагрузка короче двух байт) до сюда не доходят: RtxReceiveStream
// отбрасывает их сам. Нам это и нужно — добивка обязана быть учтена в отчёте о доставке и не
// обязана попадать в сборку кадров.
class OmniRtxMediaSink final : public webrtc::RtpPacketSinkInterface {
public:
    explicit OmniRtxMediaSink(LibWebRtcVideoSession* owner) : owner_(owner) {}

    void OnRtpPacket(const webrtc::RtpPacketReceived& packet) override;

private:
    LibWebRtcVideoSession* owner_;
};

// ТРАНСПОРТ ПРИЁМНОЙ СТОРОНЫ: ВЫПУСКАЕТ НАСТОЯЩИЙ RTCP.
//
// До #57 приёмник сообщал о потерях и просил ключевой кадр СТРУКТУРНЫМИ СОБЫТИЯМИ через
// управляющий канал оболочки, а отправитель разбирал их своими руками. Это работало, но означало,
// что мы держим собственный протокол обратной связи рядом с готовым и стандартным, и что
// апстримовый отправитель, который умеет есть только байты RTCP, к нам не подключается.
//
// Теперь приёмник отдаёт настоящие байты RTCP тем же путём, что и видео (OmniPacketSink), а дальняя
// сторона отличает их по типу пакета (см. LooksLikeRtcp). Труба одна, как и была.
class OmniRtcpTransport final : public webrtc::Transport {
public:
    explicit OmniRtcpTransport(LibWebRtcVideoSession* owner) : owner_(owner) {}

    // Приёмный модуль медиа не отправляет. Если это когда-нибудь случится, лучше узнать сразу.
    bool SendRtp(std::span<const uint8_t> /*packet*/,
                 const webrtc::PacketOptions& /*options*/) override
    {
        return false;
    }

    bool SendRtcp(std::span<const uint8_t> packet,
                  const webrtc::PacketOptions& /*options*/) override;

private:
    LibWebRtcVideoSession* owner_;
};

// ТРАНСПОРТ ДЛЯ АПСТРИМОВОГО RTP-ОТПРАВИТЕЛЯ (#50, этап 1, шаг 2).
//
// Это и есть весь интерфейс, который libwebrtc требует от транспорта: две функции на байтах.
// Наш SendRtp отдаёт байты в ядро сразу (OmniPacketSink). Ожидание, что апстрим потребует владеть
// сокетом, оказалось ложным (см. ADR, «Оценка перехода на апстримовый RTP-отправитель»).
class OmniModuleTransport final : public webrtc::Transport {
public:
    explicit OmniModuleTransport(LibWebRtcVideoSession* owner) : owner_(owner) {}

    bool SendRtp(std::span<const uint8_t> packet,
                 const webrtc::PacketOptions& /*options*/) override;

    // RTCP ОТПРАВИТЕЛЯ (SR, ответы DLRR на RRTR приёмника) — В ПРОВОД, тем же путём, что RTCP
    // приёмника (ABI 17). До этого модуль его глушил: шаг #50 был добавляющим, а та же информация
    // ходила структурным событием через оболочку. Без SR у приёмника нет LSR для RR, без DLRR —
    // своего RTT; оболочка возила вместо этого RTT между половинами сессии.
    bool SendRtcp(std::span<const uint8_t> packet,
                  const webrtc::PacketOptions& /*options*/) override;

private:
    LibWebRtcVideoSession* owner_;
};

class LibWebRtcVideoSession {
public:
    static std::unique_ptr<LibWebRtcVideoSession> Create(
        std::string_view config_json,
        std::string& error)
    {
        std::unique_ptr<LibWebRtcVideoSession> session(new LibWebRtcVideoSession());
        if (!session->Initialize(config_json, error)) {
            return nullptr;
        }

        return session;
    }

    ~LibWebRtcVideoSession()
    {
        // Wave 6 (2026-05-18): tear adaptation down BEFORE encoder/decoder.
        // OveruseFrameDetector binds to adaptation_task_queue_ and schedules
        // CheckForOveruse on it; we must stop the periodic task before the
        // queue is destroyed, otherwise the next tick fires into a dead
        // observer. QualityScaler is *constructed* on the task queue and
        // its CheckQpTask reposts via TaskQueueBase::Current() — so it must
        // also be DESTROYED on the task queue, or its dtor accesses a
        // sequence checker bound to a thread that's no longer running it.
        //
        // Order:
        //   1. Stop OveruseFrameDetector's periodic task (sync — safe; just
        //      cancels the RepeatingTaskHandle).
        //   2. Post destruction of quality_scaler_ to the task queue (the
        //      lambda owns the unique_ptr, so it'll run on-queue then drop).
        //   3. Reset adaptation_task_queue_ — its TaskQueueDeleter blocks
        //      until all queued tasks (including the scaler destruction
        //      lambda) drain. Now the queue is gone and no scheduled task
        //      can reach the observer.
        //   4. Destroy overuse_detector_ (no more pending callbacks).
        //   5. Destroy observers.
        // Wave 5b (2026-05-18): NackRequester + NackPeriodicProcessor both
        // bound their sequence checkers to adaptation_task_queue_ in
        // PostTask construction. They MUST die on that queue too. Move
        // the unique_ptrs into a lambda so destruction runs on-queue;
        // the subsequent adaptation_task_queue_.reset() drains the queue
        // and blocks until the lambda completes.
        // ЧЛЕНЫ, ПРИВЯЗАННЫЕ К ЭТОЙ ОЧЕРЕДИ, ОБНУЛЯЮТСЯ НА НЕЙ ЖЕ.
        //
        // СРАЗУ ОГОВОРКА, ЧТОБЫ НЕ ВВОДИТЬ В ЗАБЛУЖДЕНИЕ: это НЕ причина #48. То падение оказалось
        // use-after-free в самом тесте (обращение к диагностике уничтоженной сессии), и стек это
        // показал однозначно. Здешняя правка — отдельный дефект, найденный ЧТЕНИЕМ по дороге, и
        // чинится он потому, что это неопределённое поведение, а не потому, что что-то падало.
        //
        // Здесь стоял `std::move(...)` прямо в захват лямбды — то есть сам указатель трогали с
        // ПОТОКА-РАЗРУШИТЕЛЯ, в то время как задачи, уже стоящие в очереди, читают тот же член на
        // потоке очереди (`if (nack_requester_) nack_requester_->UpdateRtt(...)` и ещё пять таких
        // мест). Несинхронизированный доступ к одному `unique_ptr` из двух потоков — гонка по
        // построению, а не по подозрению: читатель может увидеть указатель посреди перемещения.
        //
        // Замысел файла всюду один: эти объекты ПРИНАДЛЕЖАТ очереди — они и создаются внутри
        // посланной задачи, и все обращения к ним идут оттуда же. Деструктор был единственным
        // местом, которое из этого замысла выпадало.
        //
        // Заодно исчезает хрупкость, ради которой раньше стоял абзац комментария: порядок смерти
        // больше не кодируется порядком объявления захватов (обратным!), а написан явно. Требование
        // прежнее — requester умирает ПЕРВЫМ, потому что его
        // ScopedNackPeriodicProcessorRegistration зовёт UnregisterNackModule у процессора; если
        // процессор уже мёртв, тот стирает end() из modules_ и получается hardened assert libc++.
        // БУФЕР ПОКАЗА УМИРАЕТ ПЕРВЫМ И НА СВОЕЙ ОЧЕРЕДИ.
        //
        // Stop() снимает сторож таймаутов и планировщик показа; без него повторяющаяся задача
        // тикнет в уже разрушенный объект. Порядок тот же, что и у всего остального в этом
        // деструкторе: что привязано к очереди — обнуляется на ней, а reset() очереди
        // дожидается, пока посланная задача отработает.
        //
        // Контроллер держит указатели на playout_timing_, playout_stats_ и playout_receiver_,
        // поэтому все трое переживают его намеренно и обнуляются ниже.
        RunOnQueueAndWait(playout_task_queue_.get(), [this] {
            if (buffer_controller_) {
                buffer_controller_->Stop();
            }
            buffer_controller_.reset();
        });
        playout_task_queue_.reset();
        playout_receiver_.reset();
        playout_stats_.reset();
        playout_timing_.reset();
        {
            std::lock_guard<std::mutex> lock(pending_decode_mutex_);
            pending_decode_frame_.reset();
        }

        RunOnQueueAndWait(adaptation_task_queue_.get(), [this] {
            nack_requester_.reset();
            nack_periodic_processor_.reset();
        });
        adaptation_task_queue_.reset();

        // Модуль умирает на СВОЕЙ очереди и по тому же правилу: то, что привязано к очереди,
        // обнуляется на ней. Транспорт переживает модуль намеренно — модуль держит на него
        // указатель и может тронуть его при разрушении.
        if (rtp_task_queue_) {
            // ЖДЁМ, А НЕ ПРОСТО ПОСЫЛАЕМ. См. RunOnQueueAndWait: reset() очередь НЕ дренирует.
            RunOnQueueAndWait(rtp_task_queue_.get(), [this] {
                // Порядок разрушения нашего пути отправки исчез вместе с самим путём: ни
                // роутера, ни пейсера, ни модуля у нас больше нет. Нумерация снимается со
                // сборщика (см. GetRtpStates ниже).
                // Приёмный модуль держит сырой указатель на статистику — гасим в этом порядке.
                receiver_rtcp_module_.reset();
                receive_statistics_.reset();
                // Сборщик умирает на своей очереди и ПОСЛЕДНИМ: его деструктор требует, чтобы
                // все созданные им отправители видео были уже сняты (RTC_DCHECK на
                // rtp_transport_controller_send.cc:143).
                // ОРКЕСТРАТОР КОДИРОВАНИЯ ГАСИТСЯ ПЕРВЫМ.
                //
                // Stop() гарантирует, что после возврата ни один закодированный кадр не придёт в
                // приёмник (так написано у него в интерфейсе). А приёмник ведёт в отправителя
                // видео, который сейчас будет снят — значит порядок именно такой и никакой другой.
                if (video_stream_encoder_) {
                    video_stream_encoder_->Stop();
                }

                if (rtp_video_sender_) {
                    // Забираем нумерацию ПЕРЕД разрушением — следующая сессия обязана её
                    // продолжить, а не начать заново под тем же SSRC.
                    const auto states = rtp_video_sender_->GetRtpStates();
                    const auto it = states.find(kOmniVideoSsrc);
                    if (it != states.end()) {
                        std::lock_guard<std::mutex> lock(g_omni_video_rtp_state_mutex);
                        g_omni_video_rtp_state = it->second;
                        g_omni_video_rtp_state_valid = true;
                    }
                }
                if (transport_controller_ && rtp_video_sender_) {
                    transport_controller_->DestroyRtpVideoSender(rtp_video_sender_);
                    rtp_video_sender_ = nullptr;
                }
                // Отправитель держал сырой указатель на переходник, переходник — на оркестратор:
                // разрушаются в обратном порядке, после отправителя.
                encoder_rtcp_feedback_.reset();
                video_stream_encoder_.reset();
                transport_controller_.reset();
                // Наблюдатель переживает сборщика: тот держит на него сырой указатель. То же и
                // с приёмником, источником и статистикой кодирования — они переживают оркестратор.
                target_rate_observer_.reset();
                encoder_sink_.reset();
                push_video_source_.reset();
                encoder_stats_observer_.reset();
                bitrate_allocator_factory_.reset();
            });
        }
        rtp_task_queue_.reset();
        module_transport_.reset();
        rtcp_transport_.reset();
        cpu_overuse_metrics_observer_.reset();
        receiver_rtcp_feedback_.reset();

        if (decoder_) {
            decoder_->Release();
        }
    }

    /// Принять кадр НОВОГО размера, не пересоздавая сессию.
    ///
    /// Раньше все три входа кодирования отвечали `INVALID_ARGUMENT`, если размер кадра не совпал
    /// с размером сессии. Значит каждая ступень лестницы ABR требовала снести сессию и собрать
    /// заново — сотни миллисекунд паузы, те самые «заминки» между ступенями качества.
    ///
    /// Верхняя граница — согласованный профиль: принимать больше, чем договорились с пиром, мы не
    /// вправе. Ниже — свободно, лестница ходит именно там.
    ///
    /// Если перенастроить не вышло, возвращаем false и вызывающий отвечает `INVALID_ARGUMENT` —
    /// ровно как прежде. Хуже, чем было, эта правка сделать не может.
    /// Принять кадр нового размера.
    ///
    /// #61 ШАГ D2b: ПЕРЕНАСТРАИВАТЬ КОДИРОВЩИК ОТСЮДА БОЛЬШЕ НЕЛЬЗЯ.
    ///
    /// Здесь стояло два пути перенастройки: у аппаратного — ReconfigureResolution на живом
    /// трансформе, у программного — повторный InitEncode. Оба лезли к кодировщику, которым теперь
    /// владеет и управляет VideoStreamEncoder: он сам замечает смену размера входящего кадра и сам
    /// пересобирает кодировщик под неё (ReconfigureEncoder по pending_encoder_reconfiguration_).
    /// Делать это за его спиной значит менять состояние объекта, который в этот момент используется
    /// другим владельцем, — и первый же замер это показал: стенд abr-follow, меняющий размер кадра
    /// на каждой ступени, перестал выдавать байты ВООБЩЕ (baseline 0, actual 0).
    ///
    /// Остаётся ровно то, что нужно нам самим: запомнить текущий размер и проверить потолок.
    bool AdaptEncoderToFrameSizeUnsafe(int width, int height)
    {
        if (width == width_ && height == height_) {
            return true;
        }
        if (width <= 0 || height <= 0 ||
            width > negotiated_width_ || height > negotiated_height_) {
            return false;
        }
        width_ = width;
        height_ = height;
        return true;
    }

    OmniWebRtcMediaStatus PushI420Frame(
        const uint8_t* y_plane,
        int32_t y_stride,
        const uint8_t* u_plane,
        int32_t u_stride,
        const uint8_t* v_plane,
        int32_t v_stride,
        int32_t width,
        int32_t height,
        int64_t timestamp_us,
        int32_t force_keyframe)
    {
        if (y_plane == nullptr || u_plane == nullptr || v_plane == nullptr ||
            width <= 0 || height <= 0) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }

        const int chroma_width = (width + 1) / 2;
        const int chroma_height = (height + 1) / 2;
        if (y_stride < width || u_stride < chroma_width || v_stride < chroma_width) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }

        // Кадр другого размера больше НЕ отвергается: сессия перенастраивает кодировщик на месте.
        // Прежний отказ вынуждал оболочку сносить и пересобирать сессию на каждой ступени ABR —
        // сотни миллисекунд паузы. Не вышло перенастроить — отвечаем как раньше.
        if (!AdaptEncoderToFrameSizeUnsafe(width, height)) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }

        webrtc::scoped_refptr<webrtc::I420Buffer> source = webrtc::I420Buffer::Create(width, height);
        CopyPlane(source->MutableDataY(), source->StrideY(), y_plane, y_stride, width, height);
        CopyPlane(source->MutableDataU(), source->StrideU(), u_plane, u_stride, chroma_width, chroma_height);
        CopyPlane(source->MutableDataV(), source->StrideV(), v_plane, v_stride, chroma_width, chroma_height);

        return EncodeVideoFrameBuffer(source, timestamp_us, force_keyframe);
    }

    /// Последняя ошибка аппаратного кодировщика — его словами.
    std::string EncoderLastError() const
    {
        if (!encoder_io_stats_) {
            return std::string();
        }
        std::lock_guard<std::mutex> lock(encoder_io_stats_->last_error_mutex);
        return encoder_io_stats_->last_error;
    }

    /// Фактический выход кодировщика за последние пять секунд, бит/с. Ноль — кадров в окне нет.
    ///
    /// ОКНО ЧИСТИТСЯ И ПРИ ЧТЕНИИ, А НЕ ТОЛЬКО ПРИ ЗАПИСИ. Первая версия чистила лишь при записи,
    /// и когда кодировщик замолкал, число ЗАМИРАЛО на последнем значении вместо того чтобы упасть
    /// в ноль — то есть пробник показывал «выход есть» ровно в том случае, ради которого он и
    /// ставился. Замер поймал это сразу: шесть отсчётов подряд дали одинаковые 344 кбит/с.
    int64_t EncodedWindowBps() const
    {
        const int64_t now_ms = env_.clock().CurrentTime().ms();
        std::lock_guard<std::mutex> lock(encoded_window_mutex_);
        while (!encoded_window_.empty() && now_ms - encoded_window_.front().at_ms > 5'000) {
            encoded_window_bytes_ -= encoded_window_.front().bytes;
            encoded_window_.pop_front();
        }
        if (encoded_window_.size() < 2) {
            return 0;
        }
        const int64_t span_ms =
            encoded_window_.back().at_ms - encoded_window_.front().at_ms;
        if (span_ms <= 0) {
            return 0;
        }
        return static_cast<int64_t>(encoded_window_bytes_) * 8 * 1000 / span_ms;
    }

    OmniWebRtcMediaStatus PushNv12Frame(
        const uint8_t* y_plane,
        int32_t y_stride,
        const uint8_t* uv_plane,
        int32_t uv_stride,
        int32_t width,
        int32_t height,
        int64_t timestamp_us,
        int32_t force_keyframe)
    {
        if (y_plane == nullptr || uv_plane == nullptr ||
            width <= 0 || height <= 0) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }

        const int chroma_width = (width + 1) / 2;
        if (y_stride < width || uv_stride < chroma_width * 2) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }

        // Кадр другого размера больше НЕ отвергается: сессия перенастраивает кодировщик на месте.
        // Прежний отказ вынуждал оболочку сносить и пересобирать сессию на каждой ступени ABR —
        // сотни миллисекунд паузы. Не вышло перенастроить — отвечаем как раньше.
        if (!AdaptEncoderToFrameSizeUnsafe(width, height)) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }

        // NV12 ОТДАЁТСЯ КАК NV12, БЕЗ ПЕРЕВОДА В I420 (#61, шаг D1).
        //
        // Раньше здесь была развилка: аппаратному кодировщику NV12 уходил как есть, программному —
        // через libyuv::NV12ToI420. Теперь путь один, а формат несёт сам кадр: аппаратная обёртка
        // возьмёт NV12 напрямую, программный кодировщик попросит ToI420 и получит его. Лишнего
        // перевода не появилось ни на одном пути — появился один выбор вместо двух веток.
        webrtc::scoped_refptr<webrtc::NV12Buffer> source =
            webrtc::NV12Buffer::Create(width, height);
        if (!source) {
            ++encode_failures_;
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }
        CopyPlane(source->MutableDataY(), source->StrideY(), y_plane, y_stride, width, height);
        CopyPlane(
            source->MutableDataUV(), source->StrideUV(), uv_plane, uv_stride,
            ((width + 1) / 2) * 2, (height + 1) / 2);

        return EncodeVideoFrameBuffer(source, timestamp_us, force_keyframe);
    }

    OmniWebRtcMediaStatus PushBgraFrame(
        const uint8_t* bgra,
        int32_t bgra_stride,
        int32_t width,
        int32_t height,
        int64_t timestamp_us,
        int32_t force_keyframe)
    {
        if (bgra == nullptr || width <= 0 || height <= 0) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }

        if (bgra_stride < width * 4) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }

        // Кадр другого размера больше НЕ отвергается: сессия перенастраивает кодировщик на месте.
        // Прежний отказ вынуждал оболочку сносить и пересобирать сессию на каждой ступени ABR —
        // сотни миллисекунд паузы. Не вышло перенастроить — отвечаем как раньше.
        if (!AdaptEncoderToFrameSizeUnsafe(width, height)) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }

        webrtc::scoped_refptr<webrtc::I420Buffer> source = webrtc::I420Buffer::Create(width, height);
        const int convert_status = libyuv::ARGBToI420(
            bgra,
            bgra_stride,
            source->MutableDataY(),
            source->StrideY(),
            source->MutableDataU(),
            source->StrideU(),
            source->MutableDataV(),
            source->StrideV(),
            width,
            height);
        if (convert_status != 0) {
            ++encode_failures_;
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }

        return EncodeVideoFrameBuffer(source, timestamp_us, force_keyframe);
    }

    // ТРИ ФУНКЦИИ EncodeHardwareH264From* УДАЛЕНЫ (#61, шаг D1).
    //
    // Каждая переводила свой формат в NV12 и звала MFT напрямую, в обход webrtc::VideoEncoder.
    // Это и был второй путь кодирования: параллельный первому, со своей сменой разрешения, своей
    // скоростью, своим ключевым кадром и своей статистикой. Аппаратный кодировщик теперь
    // webrtc::VideoEncoder (OmniHardwareH264Encoder), и перевод формата живёт внутри него — там,
    // где ему и место.

    /// Отдать кадр ОРКЕСТРАТОРУ КОДИРОВАНИЯ (#61, шаг D2b).
    ///
    /// Здесь была вся наша обвязка кодирования: отметка кадра детектору перегрузки, выбор
    /// ключевого кадра, вызов encoder_->Encode, разбор результата из очереди обратного вызова,
    /// отчёт масштабировщику качества. Всё это делает VideoStreamEncoder — и делает полнее: он же
    /// решает, не пора ли уменьшить кадр или частоту, чего наша обвязка не умела и о чём просила
    /// оболочку отдельным путём.
    ///
    /// От нас остаётся только положить кадр в источник.
    OmniWebRtcMediaStatus EncodeVideoFrameBuffer(
        const webrtc::scoped_refptr<webrtc::VideoFrameBuffer>& source,
        int64_t timestamp_us,
        int32_t force_keyframe)
    {
        if (!push_video_source_) {
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }

        int fps_snapshot = 30;
        {
            std::lock_guard<std::mutex> lock(mutex_);
            fps_snapshot = fps_;
        }
        const int64_t frame_timestamp_us = timestamp_us > 0
            ? timestamp_us
            : static_cast<int64_t>(++synthetic_frame_index_) * 1'000'000 / fps_snapshot;
        const uint32_t rtp_timestamp = static_cast<uint32_t>((frame_timestamp_us * 90) / 1000);

        // ПРОСЬБА О КЛЮЧЕВОМ КАДРЕ АДРЕСУЕТСЯ ОРКЕСТРАТОРУ, А НЕ КОДИРОВЩИКУ.
        //
        // Он знает, сколько потоков и слоёв, и переведёт просьбу в нужные VideoFrameType сам.
        if (force_keyframe != 0 || ConsumePendingKeyframeRequest() || frames_encoded_ == 0) {
            if (rtp_task_queue_) {
                rtp_task_queue_->PostTask([this] {
                    if (video_stream_encoder_) {
                        video_stream_encoder_->SendKeyFrame();
                    }
                });
            }
        }

        // ВРЕМЯ СЪЁМКИ СТАВИТСЯ ЯВНО, И ЭТО НЕ ФОРМАЛЬНОСТЬ (#61, шаг D2b).
        //
        // Метку RTP оркестратор кодирования ставит САМ и нашу перезаписывает: он выводит её из
        // времени съёмки кадра (video_stream_encoder.cc:1640). Не дать ему это время — значит
        // получить метку от стенных часов, никак не связанную с нашим захватом: и стенд не
        // сопоставит принятый кадр с исходным, и липсинк не сведёт звук с картинкой. Поэтому
        // ставим и timestamp_us, и timestamp_ms — второе и есть то, что он читает.
        webrtc::VideoFrame frame = webrtc::VideoFrame::Builder()
            .set_video_frame_buffer(source)
            .set_rtp_timestamp(rtp_timestamp)
            .set_timestamp_us(frame_timestamp_us)
            .set_timestamp_ms(frame_timestamp_us / 1000)
            .build();

        // ОКНО МЕЖДУ СОЗДАНИЕМ СЕССИИ И ЗАПУСКОМ ОРКЕСТРАТОРА.
        //
        // Оркестратор строится задачей на rtp-очереди, а первый кадр приходит сразу за созданием
        // сессии. Пока источник не подписан, брать кадр некому — и терялся бы ровно КЛЮЧЕВОЙ, а с
        // ним SPS и PPS. Ту же цену мы уже платили на перевороте C2b. Поэтому до подписки кадры
        // едут через ту же очередь и приходят в правильном порядке.
        if (!encoder_source_ready_.load(std::memory_order_acquire)) {
            if (!rtp_task_queue_) {
                ++encode_failures_;
                return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
            }
            rtp_task_queue_->PostTask([this, frame] {
                if (push_video_source_) {
                    (void)push_video_source_->PushFrame(frame);
                }
            });
            return OMNI_WEBRTC_MEDIA_STATUS_OK;
        }

        if (!push_video_source_->PushFrame(frame)) {
            ++encode_failures_;
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }
        return OMNI_WEBRTC_MEDIA_STATUS_OK;
    }

    OmniWebRtcMediaStatus SetRates(int32_t bitrate_bps, int32_t fps)
    {
        if (bitrate_bps <= 0 || fps <= 0) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }

        const int clamped_bitrate_bps = ClampInt(bitrate_bps, 64'000, 20'000'000);
        const int clamped_fps = ClampInt(fps, 1, 120);

        {
            std::lock_guard<std::mutex> lock(mutex_);
            bitrate_bps_ = clamped_bitrate_bps;
            fps_ = clamped_fps;
            abr_target_bitrate_bps_ = clamped_bitrate_bps;
            abr_target_fps_ = clamped_fps;
        }

        // РЕШЕНИЕ ОБОЛОЧКИ ДОХОДИТ ДО КОДИРОВЩИКА ЧЕРЕЗ ОРКЕСТРАТОР (#61, шаг D2b).
        //
        // Здесь стоял прямой вызов кодировщику: SetRates на аппаратном или на программном, плюс
        // синхронизация частоты с детектором перегрузки. Кодировщиком владеет VideoStreamEncoder,
        // и трогать его настройки за его спиной нельзя — он сам решает, какую скорость и какой
        // размер просить, сводя вместе оценку канала, перегрузку процессора и качество картинки.
        //
        // Наше дело — сказать ему ПОТОЛОК, выбранный отправителем: это конфигурация, и меняется
        // она ровно так же, как при создании.
        if (rtp_task_queue_) {
            rtp_task_queue_->PostTask([this] {
                if (video_stream_encoder_) {
                    video_stream_encoder_->ConfigureEncoder(
                        BuildEncoderConfigUnsafe(), max_rtp_packet_bytes_);
                }
            });
        }

        std::lock_guard<std::mutex> lock(mutex_);
        // ТЕМП ПЕЙСЕРА МЫ БОЛЬШЕ НЕ ТРОГАЕМ ВООБЩЕ (#57, шаг C2b).
        //
        // Здесь стояла оговорка «ставим свой темп, пока GoogCC не высказался». Она была нужна,
        // пока пейсеров было два и оба наши. Теперь пейсер один и принадлежит сборщику, а темп ему
        // назначает его же регулятор. Любая запись отсюда была бы возвратом ровно к той поломке,
        // ради которой оговорка и появилась: самопись, глушащая регулятор.
        ++rate_updates_;
        return OMNI_WEBRTC_MEDIA_STATUS_OK;
    }

    // НОВЫЙ ПОТОЛОК НА ЖИВОЙ СЕССИИ — без пересоздания (настройка «Битрейт», 2026-10-18).
    //
    // Раньше смена потолка шла пересозданием отправителя целиком: ~1 с без кадров, ключевой кадр и
    // старт сразу с потолка (живой звонок: `6300 -> 5153 down:congested` на первом же такте). Потолок
    // — это конфигурация, а не состояние: регулятору новые границы (при снижении start = -1 —
    // оценка канала остаётся, upstream RtpBitrateConfigurator; при подъёме — старт с текущей цели и
    // пробы вверх), оркестратору кодирования новый максимум, своему ABR — новый зажим. Цель ниже
    // нового потолка не трогается, выше — опускается сразу.
    OmniWebRtcMediaStatus SetMaxBitrate(int32_t max_bitrate_bps)
    {
        if (max_bitrate_bps <= 0) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }

        const int ceiling_bps = ClampInt(max_bitrate_bps, kAbrMinBitrateBps, 20'000'000);
        webrtc::VideoEncoderConfig encoder_config;
        // A RAISED ceiling restarts the controller from where it is, not from scratch: a new start
        // makes it probe up (x3, x6) as at call start. Only widening the bounds left it creeping —
        // live 2026-10-18, screen 2100 -> 6300 kbps: +134 kbps in 14 s after sitting at the old cap.
        // A lowered ceiling keeps the estimate (start = -1).
        int restart_from_bps = -1;
        {
            std::lock_guard<std::mutex> lock(mutex_);
            if (ceiling_bps > negotiated_bitrate_bps_ && abr_target_bitrate_bps_ > 0) {
                restart_from_bps = std::min(abr_target_bitrate_bps_, ceiling_bps);
            }
            negotiated_bitrate_bps_ = ceiling_bps;
            abr_target_bitrate_bps_ = std::min(abr_target_bitrate_bps_, ceiling_bps);
            bitrate_bps_ = std::min(bitrate_bps_, ceiling_bps);
            last_configured_bitrate_bps_ = bitrate_bps_;
            encoder_config = BuildEncoderConfigUnsafe();
            ++rate_updates_;
        }

        if (rtp_task_queue_) {
            rtp_task_queue_->PostTask(
                [this, ceiling_bps, restart_from_bps, config = std::move(encoder_config)]() mutable {
                    if (transport_controller_) {
                        webrtc::BitrateConstraints constraints;
                        constraints.min_bitrate_bps = kAbrMinBitrateBps;
                        constraints.start_bitrate_bps = restart_from_bps;
                        constraints.max_bitrate_bps = ceiling_bps;
                        transport_controller_->SetSdpBitrateParameters(constraints);
                    }
                    if (video_stream_encoder_) {
                        video_stream_encoder_->ConfigureEncoder(
                            std::move(config), max_rtp_packet_bytes_);
                    }
                });
        }

        return OMNI_WEBRTC_MEDIA_STATUS_OK;
    }

    OmniWebRtcMediaStatus RequestKeyframe()
    {
        // ПРОСЬБА АДРЕСУЕТСЯ ОРКЕСТРАТОРУ, А НЕ КОДИРОВЩИКУ (#61, шаг D2b).
        //
        // Он знает, сколько потоков и слоёв, и переведёт просьбу в нужные VideoFrameType сам.
        if (rtp_task_queue_) {
            rtp_task_queue_->PostTask([this] {
                if (video_stream_encoder_) {
                    video_stream_encoder_->SendKeyFrame();
                }
            });
        }

        std::lock_guard<std::mutex> lock(mutex_);
        force_next_keyframe_ = true;
        ++keyframe_requests_;
        return OMNI_WEBRTC_MEDIA_STATUS_OK;
    }

    OmniWebRtcMediaStatus PushRtpPackets(
        const uint8_t* buffer,
        size_t buffer_len,
        uint32_t packet_count)
    {
        if (buffer == nullptr || buffer_len == 0) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }

        size_t offset = 0;
        uint32_t parsed_packets = 0;
        while (offset < buffer_len && (packet_count == 0 || parsed_packets < packet_count)) {
            if (buffer_len - offset < 2u) {
                return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
            }

            const uint16_t packet_len = ReadLittleEndian16(buffer + offset);
            offset += 2u;
            if (packet_len == 0 || buffer_len - offset < packet_len) {
                return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
            }

            const OmniWebRtcMediaStatus status = PushRtpPacket(buffer + offset, packet_len);
            if (status != OMNI_WEBRTC_MEDIA_STATUS_OK) {
                return status;
            }

            offset += packet_len;
            ++parsed_packets;
        }

        if (packet_count != 0 && parsed_packets != packet_count) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }

        return offset == buffer_len
            ? OMNI_WEBRTC_MEDIA_STATUS_OK
            : OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }

    OmniWebRtcMediaStatus UpdateAbr(
        const OmniWebRtcMediaAbrSample* sample,
        OmniWebRtcMediaAbrTarget* target)
    {
        if (sample == nullptr || target == nullptr) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }

    // RTT ИЗ ЭТОЙ ВЫБОРКИ БОЛЬШЕ НЕ ИДЁТ В NackRequester — ПРОВОД БЫЛ МЁРТВЫЙ.
    //
    // Здесь стояла подача `sample->rtt_ms` (EWMA по RTCP RR) в `NackRequester::UpdateRtt`.
    // Смысл у неё был бы, если бы её слушал тот запросчик, который реально переспрашивает. Но
    // `update_abr` оболочка зовёт на ОТПРАВЛЯЮЩЕЙ сессии, а пакеты подаёт только в ПРИЁМНУЮ
    // (`PushRtpPackets` -> TrackReceiverPacketUnsafe -> OnReceivedPacket). Запросчик отправляющей
    // сессии не видит ни одного пакета за звонок: переспрашивать ему нечего, и RTT ему незачем.
    //
    // Работающему запросчику RTT приходит своим путём и лучшего качества: оболочка снимает
    // проход туда-обратно с отправляющей половины (`transport_rtt_us`, измерен по ФАКТИЧЕСКОЙ
    // доставке из отчётов RFC 8888); у приёмной свой RTT, из XR её модуля. См. OmniRttObserver.
    //
    // Само поле в выборке осталось: оно кормит тренды в диагностике (`abr_rtt_ewma_ms`).

        webrtc::VideoEncoderConfig pending_encoder_config;
        bool reconfigure_encoder = false;
        {
            std::lock_guard<std::mutex> lock(mutex_);
            const int64_t now_ms = EffectiveNowMs(sample->now_ms);

            // Wave 4 Phase 2b (2026-05-16): route the sample through GoogCC
            // for bitrate estimation. Output overrides the legacy state
            // machine's bitrate; legacy logic still drives FPS/resolution
            // (Omni-policy) and state classification for diagnostics.
            const int googcc_target_bps = RouteSampleThroughGoogCcUnsafe(now_ms);

            // Тренды считаем, но решения по ним больше не принимаем: они остаются входом
            // диагностики (abr_*_ewma_* в diagnostics_json), чтобы человек видел, что происходило
            // с каналом, даже когда автоматика на это не реагирует отдельно. Источник — последний
            // RR собеседника; RTT — свой по отчётам о доставке, если он есть (точнее), иначе из RR.
            const int64_t trend_rtt_ms = transport_rtt_us_ >= 0
                ? transport_rtt_us_ / 1000
                : std::max<int64_t>(0, sender_report_blocks_.rtt_ms());
            UpdateAbrTrendUnsafe(
                static_cast<double>(sender_report_blocks_.fraction_lost_q8()),
                static_cast<double>(trend_rtt_ms),
                static_cast<double>(sender_report_blocks_.jitter()));

            // ДАВНОСТЬ ОБРАТНОЙ СВЯЗИ — ЗДЕСЬ, ПО ФАКТУ ПРИХОДА ОТЧЁТОВ (ABI 18).
            //
            // Раньше её считала оболочка (CallVideoFeedbackStaleness) по моменту прихода
            // структурного события RR. Определение то же: пока отчётов не было ни одного, отсчёт
            // идёт от начала отправки — молчание собеседника не «ноль миллисекунд назад», а
            // «столько, сколько мы шумим». Часы — те же, что у такта (у стендов свои).
            const uint64_t feedback_now =
                sender_report_blocks_.reports() + transport_feedback_reports_applied_;
            if (feedback_now != abr_feedback_seen_) {
                abr_feedback_seen_ = feedback_now;
                abr_last_feedback_ms_ = now_ms;
            }
            if (abr_sending_since_ms_ == 0 && rtp_packets_sent_.load(std::memory_order_relaxed) > 0) {
                abr_sending_since_ms_ = now_ms;
            }
            const int64_t feedback_since =
                abr_last_feedback_ms_ > 0 ? abr_last_feedback_ms_ : abr_sending_since_ms_;
            const int64_t feedback_stale_ms = feedback_since > 0 ? now_ms - feedback_since : 0;

            // РЕГУЛЯТОР СКОРОСТИ ОДИН — GoogCC.
            //
            // Здесь стояла своя лестница: пороги по потерям (64/26/8), RTT (350/180/120), джиттеру
            // (4500), длине очереди (768/512/384), EWMA-окна, счётчики подряд идущих проб и семь
            // состояний. Всё это снято, и вот почему.
            //
            // Каждый из перечисленных сигналов — ВХОД GoogCC, а не отдельное основание для решения.
            // Внутри того самого объекта, который мы уже создаём, лежат проверенные реализации
            // ровно этих политик:
            //   потери         -> LossBasedBweV2 + SendSideBandwidthEstimation
            //   рост RTT       -> RttBasedBackoff (включён по умолчанию: лимит 3 с, сброс на 80%
            //                     раз в секунду, пол 5 кбит/с)
            //   длина очереди  -> CongestionWindowPushbackController (он и написан для того, чтобы
            //                     давить прямо на цель кодировщика)
            //   ёмкость канала -> LinkCapacityEstimator
            //   выход из ямы   -> ProbeController, причём ПРЫЖКОМ, а не ступенями
            // Мы скармливали GoogCC те же самые числа и одновременно решали по ним сами — двойной
            // учёт. Хуже того, самопись глушила регулятор в трёх местах:
            //   1. При неподтверждённом заторе делался ранний return, и оценка GoogCC за этот такт
            //      не применялась вовсе. На «мигающем» канале это могло съедать большинство тактов.
            //   2. Потолок abr_measured_capacity_bps_ был храповиком: вниз — по любой пробе с
            //      затором, вверх — не больше чем на 25% за стабильное окно >= 4 с. От пола
            //      150 кбит/с до 8 Мбит/с это 18 шагов, то есть больше 70 секунд ползком. Этот же
            //      храповик обезвреживал пробы ProbeController, которые и существуют, чтобы
            //      выходить из ямы прыжком.
            //   3. Ниже по функции своя настройка пейсера затирала pacer_config от GoogCC.
            //
            // Обстоятельство, которое всё объясняет: до 2026-08-18 мы не звали OnSentPacket. Без
            // него у GoogCC не взводится RttBasedBackoff (last_packet_sent_ остаётся -inf), не
            // определяется ALR и не обновляется congestion window. Регулятор работал полуслепым, и
            // лестница компенсировала именно это. Вход подключён — компенсация стала вредной.
            //
            // ЛОКАЛЬНАЯ ПЕРЕГРУЗКА ТОЖЕ БОЛЬШЕ НЕ НАША (#61, шаг D3).
            //
            // Здесь стояло правило по времени захвата и кодирования из замера оболочки: если кадр
            // готовится дольше межкадрового интервала, а отправляется кадров меньше запрошенного,
            // считаем себя перегруженными и режем частоту. Живой замер 2026-08-17 (город А <->
            // город Б, 121 решение ABR) показал, что условие истинно ВСЕГДА: пороги 33-41 мс против
            // замеренных 47-91 мс на захвате и 55-97 мс на кодировании — и на 320x180 тоже. Второй
            // его половиной служил низкий sent_fps, который у демонстрации экрана означает «на
            // экране ничего не изменилось», а вовсе не «мы не успеваем».
            //
            // Ту же ось апстрим меряет иначе и по существу: OveruseFrameDetector внутри
            // оркестратора считает не «сколько миллисекунд», а ДОЛЮ времени, которую кодирование
            // занимает от межкадрового интервала, ведёт по ней скользящее окно и знает, что
            // аппаратный кодировщик конвейерный — ему пороги вдвое выше (150/200% против 42/85%).
            // Замеряет он при этом СВОЙ кодировщик, а не пересказ оболочки через FFI.

            const int previous_bitrate = abr_target_bitrate_bps_;
            const int previous_fps = abr_target_fps_;
            const int previous_width = abr_target_width_;
            const int previous_height = abr_target_height_;

            // Цель по битрейту — то, что сказал GoogCC, зажатое ТОЛЬКО выбором отправителя.
            // Своего потолка «по измеренной ёмкости» здесь больше нет: измерять ёмкость — работа
            // LinkCapacityEstimator внутри регулятора, и он делает это с учётом проб и задержки, а
            // не по одному факту «на этой скорости однажды стало плохо». Границы регулятор
            // знает сам: min/max/starting заданы в NetworkControllerConfig.constraints при его
            // создании, а negotiated_bitrate_bps_ после конструктора не меняется.
            if (googcc_target_bps > 0) {
                abr_target_bitrate_bps_ = ClampInt(
                    googcc_target_bps,
                    kAbrMinBitrateBps,
                    std::max(kAbrMinBitrateBps, negotiated_bitrate_bps_));
                if (abr_target_bitrate_bps_ < previous_bitrate) {
                    abr_state_ = OMNI_WEBRTC_MEDIA_ABR_STATE_DOWN_CONGESTED;
                    SetAbrReasonUnsafe("googcc:down");
                } else if (abr_target_bitrate_bps_ > previous_bitrate) {
                    abr_state_ = OMNI_WEBRTC_MEDIA_ABR_STATE_UP_STABLE;
                    SetAbrReasonUnsafe("googcc:up");
                } else {
                    abr_state_ = OMNI_WEBRTC_MEDIA_ABR_STATE_STABLE;
                    SetAbrReasonUnsafe("googcc:hold");
                }
            } else {
                // Регулятор мнения не имеет. Два случая, и оба надо НАЗЫВАТЬ, а не молчать:
                // сборка без libwebrtc (GoogCC в ней нет вовсе, и адаптации не будет никакой) или
                // первые такты звонка, пока он не набрал данных. Ровная линия без объяснения в
                // диагностике — это ровно то, из-за чего в этом файле уже был день поисков
                // виноватого не там.
                abr_state_ = OMNI_WEBRTC_MEDIA_ABR_STATE_STABLE;
                SetAbrReasonUnsafe("no-estimate");
            }

            // ОБРАТНАЯ СВЯЗЬ ЗАМОЛЧАЛА — ТЕПЕРЬ ЭТО РАБОТА РЕГУЛЯТОРА, КРОМЕ ОДНОГО СЛУЧАЯ
            // (#61, шаг D3).
            //
            // Правило родилось из живого замера, где звонок топил мобильный аплинк: 1.15 МБ за 4 с,
            // RTT 134 -> 808 мс и двадцать секунд, в течение которых собеседник не получал ничего,
            // а мы не сбавляли (docs/bugs/call-drowns-the-mobile-uplink.md). Требование верное и
            // остаётся в силе; сменился исполнитель.
            //
            // У апстрима это RttBasedBackoff: пока отчёты не приходят, а пакеты уходят, его
            // «исправленный RTT» растёт сам (время с последнего отчёта плюс последний RTT), и через
            // три секунды оценка режется на 20% в секунду. Раньше полагаться на это было нельзя,
            // потому что мы не звали OnSentPacket и механизм не взводился. Теперь путь отправки
            // целиком апстримовский, и механизм ЗАМЕРЕН: стенд RunFeedbackSilenceSmoke держит наше
            // правило разоружённым (feedback_stale_ms = 0 весь прогон) и показывает 534720 ->
            // 150000 бит/с за 12 секунд молчания.
            //
            // ОСТАВШИЙСЯ СЛУЧАЙ — «ОТЧЁТОВ НЕ БЫЛО НИ ОДНОГО». Его апстрим не покрывает НАМЕРЕННО:
            // last_propagation_rtt_update_ инициализируется бесконечностью, «чтобы откат по RTT не
            // срабатывал, пока обратная связь по пакетам не включена»
            // (send_side_bandwidth_estimation.cc:120). То есть если пир не сказал ни слова с самого
            // начала, регулятор не сбавит никогда — а мы в этом же случае и его мнение не берём
            // (см. delivery_proven в RouteSampleThroughGoogCcUnsafe). Без этой ветки звонок с
            // мёртвой обратной связью держал бы стартовую скорость до конца.
            //
            // Логика намеренно однонаправленная: пока пир молчит, поднимать нельзя НИКОГДА, а
            // снижать надо активно — наши же байты и держат путь забитым, поэтому «подождём и
            // посмотрим» здесь означает «подождём и останемся сломанными».
            if (feedback_stale_ms >= kAbrFeedbackLostMs &&
                transport_feedback_reports_applied_ == 0) {
                const int decayed = static_cast<int>(
                    static_cast<int64_t>(previous_bitrate) * 85 / 100);
                abr_target_bitrate_bps_ = std::max(
                    kAbrMinBitrateBps,
                    std::min(abr_target_bitrate_bps_, decayed));
                abr_state_ = OMNI_WEBRTC_MEDIA_ABR_STATE_DOWN_CONGESTED;
                SetAbrReasonUnsafe("down:feedback_never_arrived");
            }

            // РАЗМЕР КАДРА И ЧАСТОТУ ВЫБИРАЕТ ОРКЕСТРАТОР КОДИРОВАНИЯ (#61, шаг D3).
            //
            // Здесь стояли две наши функции от битрейта: ступень разрешения по таблице
            // EncoderInfoSettings и частота, пропорционально урезанная, если битрейта не хватает
            // даже на минимум для выбранного размера. Обе снимаются целиком.
            //
            // Ту же работу делает VideoStreamEncoderResourceManager, и делает её по ФАКТУ, а не по
            // просьбе:
            //   перегрузка процессора -> EncodeUsageResource (доля времени кодирования)
            //   рост QP               -> QualityScalerResource (и программный, и аппаратный путь:
            //                            аппаратному QP достаёт разбор битстрима, см.
            //                            GetEncoderInfo у OmniHardwareH264Encoder)
            // Решение он выражает пожеланиями источнику — не больше стольких точек и не чаще
            // стольких кадров, — и это ровно та величина, которую наш FFI отдаёт оболочке как цель.
            //
            // Разница не в источнике чисел, а в том, ЧТО измеряется. Наша ступень считалась от
            // ПРОСЬБЫ к кодировщику; апстримовская — от того, сколько он отдал на самом деле.
            // Промах между этими двумя величинами и был причиной, по которой мы весь 2026-08-17
            // искали виноватого в кодировщике: при цели 150 кбит/с аппаратный H.264 клал 605.
            ApplyEncoderSinkWantsUnsafe();
            NoteEncoderAdaptationUnsafe();

            const bool changed = previous_bitrate != abr_target_bitrate_bps_ ||
                                 previous_fps != abr_target_fps_ ||
                                 previous_width != abr_target_width_ ||
                                 previous_height != abr_target_height_;
            FillAbrTargetUnsafe(*target, changed);

            // ПЕРЕНАСТРОЙКА КОДИРОВЩИКА — ТОЛЬКО КОГДА ПОТОЛОК ДЕЙСТВИТЕЛЬНО СДВИНУЛСЯ.
            //
            // Здесь стояло «на каждом замере», и обоснование было такое: между вызовами
            // EncoderBitrateAdjuster успевает увидеть фактический выход и поправить просьбу, а
            // при условии «если изменилось» поправка замирала на полу. Обоснование УСТАРЕЛО вместе
            // с шагом D2b: своего корректора у нас больше нет, скорость кодировщику даёт регулятор
            // событийно (ApplyTargetRateToEncoder), а конфигурация несёт ровно одно — ПОТОЛОК.
            // Повторять неизменившийся потолок незачем.
            //
            // Цена «незачем» оказалась не нулевой. Живая оболочка зовёт замер ABR 3-4 раза в
            // секунду (трасса media/abr, 2026-08-27), то есть кодировщик перенастраивался столько
            // же раз — а каждая перенастройка у апстрима это полный ReconfigureEncoder на очереди
            // кодирования, вплоть до пересоздания кодировщика. Ради повторения одного и того же
            // числа.
            //
            // Апстрим устроен так же: приложение задаёт конфигурацию, когда ОНО передумало, а не
            // по таймеру (VideoSendStream::ReconfigureVideoEncoder). Событийная подача скорости
            // от регулятора при этом никуда не девается и идёт как шла.
            bitrate_bps_ = abr_target_bitrate_bps_;
            fps_ = abr_target_fps_;
            ++rate_updates_;
            if (bitrate_bps_ != last_configured_bitrate_bps_ ||
                fps_ != last_configured_fps_) {
                last_configured_bitrate_bps_ = bitrate_bps_;
                last_configured_fps_ = fps_;
                // КОНФИГУРАЦИЯ СТРОИТСЯ ЗДЕСЬ, ПОД МЬЮТЕКСОМ, И ЕДЕТ В ЗАДАЧУ ЗНАЧЕНИЕМ.
                //
                // Раньше `BuildEncoderConfigUnsafe()` звался ВНУТРИ задачи — то есть на
                // rtp-очереди и без мьютекса, хотя суффикс Unsafe означает ровно обратное. Он
                // читает bitrate_bps_ и fps_, а писались они здесь, ПОСЛЕ постановки задачи:
                // кодировщик получал то старое число, то новое, как повезёт с планировщиком.
                pending_encoder_config = BuildEncoderConfigUnsafe();
                reconfigure_encoder = true;
            }
        }

        if (reconfigure_encoder && rtp_task_queue_) {
            rtp_task_queue_->PostTask(
                [this, config = std::move(pending_encoder_config)]() mutable {
                    if (video_stream_encoder_) {
                        video_stream_encoder_->ConfigureEncoder(
                            std::move(config), max_rtp_packet_bytes_);
                    }
                });
        }

        return OMNI_WEBRTC_MEDIA_STATUS_OK;
    }

    OmniWebRtcMediaStatus DrainI420Frame(
        uint8_t* buffer,
        size_t buffer_len,
        int32_t* width,
        int32_t* height,
        uint32_t* rtp_timestamp,
        int64_t* timestamp_us,
        size_t* required_len)
    {
        if (width == nullptr || height == nullptr || rtp_timestamp == nullptr ||
            timestamp_us == nullptr || required_len == nullptr) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }

        *width = 0;
        *height = 0;
        *rtp_timestamp = 0;
        *timestamp_us = 0;
        *required_len = 0;

        DecodePendingPlayoutFrame();

        DecodedI420Frame frame;
        if (!decode_callback_.Peek(frame)) {
            return OMNI_WEBRTC_MEDIA_STATUS_OK;
        }

        *width = frame.width;
        *height = frame.height;
        *rtp_timestamp = frame.rtp_timestamp;
        *timestamp_us = frame.timestamp_us;
        *required_len = frame.data.size();
        if (buffer == nullptr || buffer_len < frame.data.size()) {
            return OMNI_WEBRTC_MEDIA_STATUS_BUFFER_TOO_SMALL;
        }

        std::memcpy(buffer, frame.data.data(), frame.data.size());
        decode_callback_.Pop();
        ++frames_drained_;
        return OMNI_WEBRTC_MEDIA_STATUS_OK;
    }

    OmniWebRtcMediaStatus DrainBgraFrame(
        uint8_t* buffer,
        size_t buffer_len,
        int32_t* width,
        int32_t* height,
        uint32_t* rtp_timestamp,
        int64_t* timestamp_us,
        size_t* required_len)
    {
        if (width == nullptr || height == nullptr || rtp_timestamp == nullptr ||
            timestamp_us == nullptr || required_len == nullptr) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }

        *width = 0;
        *height = 0;
        *rtp_timestamp = 0;
        *timestamp_us = 0;
        *required_len = 0;

        DecodePendingPlayoutFrame();

        DecodedI420Frame frame;
        if (!decode_callback_.Peek(frame)) {
            return OMNI_WEBRTC_MEDIA_STATUS_OK;
        }

        const size_t required = static_cast<size_t>(frame.width) * static_cast<size_t>(frame.height) * 4u;
        *width = frame.width;
        *height = frame.height;
        *rtp_timestamp = frame.rtp_timestamp;
        *timestamp_us = frame.timestamp_us;
        *required_len = required;
        if (buffer == nullptr || buffer_len < required) {
            return OMNI_WEBRTC_MEDIA_STATUS_BUFFER_TOO_SMALL;
        }

        const int chroma_width = (frame.width + 1) / 2;
        const int chroma_height = (frame.height + 1) / 2;
        const size_t y_size = static_cast<size_t>(frame.width) * static_cast<size_t>(frame.height);
        const size_t uv_size = static_cast<size_t>(chroma_width) * static_cast<size_t>(chroma_height);
        const uint8_t* y_plane = frame.data.data();
        const uint8_t* u_plane = y_plane + y_size;
        const uint8_t* v_plane = u_plane + uv_size;
        const int convert_status = libyuv::I420ToARGB(
            y_plane,
            frame.width,
            u_plane,
            chroma_width,
            v_plane,
            chroma_width,
            buffer,
            frame.width * 4,
            frame.width,
            frame.height);
        if (convert_status != 0) {
            ++decode_failures_;
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }

        decode_callback_.Pop();
        ++frames_drained_;
        (void)chroma_height;
        return OMNI_WEBRTC_MEDIA_STATUS_OK;
    }

#if defined(_WIN32)
    // #5 increment 3a (2026-06-01): GPU video renderer attach/render/resize/detach. The C#
    // layer creates a SwapChainPanel, binds the returned swapchain via ISwapChainPanelNative,
    // then calls RenderAttachedVideo on its render tick. Decoded frames render straight to the
    // swapchain backbuffer (no BGRA round-trip across FFI). The visible Present is validated
    // live.
    //
    // Threading contract: these calls must be SERIALISED against each other (the caller's lock),
    // but they do not have to come from one particular thread, and RenderAttachedVideo must not
    // come from the UI thread. The D3D11 device is created multithread-protected — it has to be
    // anyway, the decoder MFT decodes on its own thread — and the only call that needs the UI
    // thread is ISwapChainPanelNative::SetSwapChain, which happens on the C# side of the attach.
    // The shell presents from a dedicated thread on the display's clock; putting the present in
    // the shell's dispatcher queue cost gaps of up to 578 ms between frames.
    // See docs/decisions/video-presentation-thread.md.
    OmniWebRtcMediaStatus AttachVideoRenderer(int32_t width, int32_t height, void** out_swapchain)
    {
        if (out_swapchain == nullptr || width <= 0 || height <= 0) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }
        // Task #87: route the receive path to the user's chosen GPU (0 = Auto = iGPU-first). Must
        // be set before the first swapchain create, since the renderer caches its device.
        video_renderer_.SetPreferredAdapterVendor(preferred_decode_vendor_id_);
        IDXGISwapChain1* swapchain = nullptr;
        if (!video_renderer_.CreateCompositionSwapChain(width, height, &swapchain)) {
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }
        *out_swapchain = swapchain;  // AddRef'd; ownership transferred to the caller
        // Task #84 increment 3: HW H.264/HEVC decode on the renderer's (video-capable) D3D11 device
        // for zero-copy decode→render. Neither has a software decoder (only AV1 does), so the
        // passport offers them only where this decoder opens.
        //
        // A decoder that already exists is KEPT. H.264 and HEVC bring it up on the first frame, on
        // this same renderer device (the device is created once and cached), before the window
        // attaches; replacing it here handed the stream's only key frame to the decoder being
        // thrown away and fed the new one delta frames it can never decode.
        const bool try_hardware = !HasSoftwareVideoCodec(codec_kind_) ||
            (codec_kind_ == NativeVideoCodec::Av1 && !hardware_av1_tried_);
        if (try_hardware && video_renderer_.device() != nullptr && !force_software_decode_ &&
            !hardware_h264_decoder_) {
            hardware_av1_tried_ = hardware_av1_tried_ || codec_kind_ == NativeVideoCodec::Av1;
            std::string decoder_error;
            hardware_h264_decoder_ = MediaFoundationH264Decoder::Create(
                video_renderer_.device(), width_, height_, decoder_error, codec_kind_);
            if (hardware_h264_decoder_ && frames_fed_to_decoder_ > 0) {
                // It joins a stream already under way (an earlier decoder failed and was dropped): it
                // has no reference frame. Wait for a key frame instead of feeding it deltas, and ask
                // for one — there is no periodic IDR (docs/decisions/keyframes-on-demand.md).
                playout_keyframe_required_.store(true, std::memory_order_relaxed);
                std::lock_guard<std::mutex> lock(mutex_);
                RequestReceiverKeyframeUnsafe(MonotonicNowMs(), kReasonDecoderStarted);
            }
        }
        return OMNI_WEBRTC_MEDIA_STATUS_OK;
    }

    OmniWebRtcMediaStatus ResizeAttachedVideo(int32_t width, int32_t height)
    {
        if (width <= 0 || height <= 0) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }
        return video_renderer_.ResizeSwapChain(width, height)
            ? OMNI_WEBRTC_MEDIA_STATUS_OK
            : OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }

    /// Render tick. `out_presented` (optional) reports whether a NEW frame actually reached
    /// the swapchain — "nothing to draw" and "drew a frame" both return OK, so without it the
    /// caller cannot tell them apart.
    ///
    /// That distinction is not cosmetic. The tick is a clock, not a frame delivery: the shell
    /// calls this on a steady display-paced beat whether or not the peer is still sending. It
    /// used to stamp "a remote frame was rendered just now" on every tick, so its staleness
    /// check never fired and the peer's last picture stayed frozen on screen for the rest of the
    /// call (live test 2026-08-11: camera stopped, image stayed; the following screen share was
    /// invisible behind it). That was true when the tick came from the inbound datagram batch —
    /// audio and video share one datagram stream — and it stays true on a timer.
    OmniWebRtcMediaStatus RenderAttachedVideo(bool* out_presented)
    {
        if (out_presented != nullptr) {
            *out_presented = false;
        }
        DecodePendingPlayoutFrame();

        // Task #84 increment 3: HW path — render the latest HW-decoded NV12 texture (zero-copy).
        // DecodeDuePlayoutFrames (above) fed the HW decoder + stored the newest frame. When the HW
        // decoder is active, decoded frames go here (not the I420 deque), so this is the only path.
        if (has_hw_nv12_frame_ && hw_nv12_render_tex_) {
            has_hw_nv12_frame_ = false;
            const bool rendered = video_renderer_.RenderNv12TextureToSwapChain(
                hw_nv12_render_tex_.Get(), 0, hw_nv12_width_, hw_nv12_height_);
            NotePresentedSize(hw_nv12_width_, hw_nv12_height_);
            if (out_presented != nullptr) {
                *out_presented = rendered;
            }
            return rendered ? OMNI_WEBRTC_MEDIA_STATUS_OK
                            : OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }

        // SW path: render the most recent decoded frame (drop older for low latency).
        DecodedI420Frame frame;
        bool have_frame = false;
        DecodedI420Frame next;
        while (decode_callback_.TryPop(next)) {
            frame = std::move(next);
            have_frame = true;
        }
        if (!have_frame) {
            return OMNI_WEBRTC_MEDIA_STATUS_OK;
        }
        if (out_presented != nullptr) {
            *out_presented = true;
        }

        const int width = frame.width;
        const int height = frame.height;
        const int chroma_width = (width + 1) / 2;
        const int chroma_height = (height + 1) / 2;
        const size_t y_size = static_cast<size_t>(width) * static_cast<size_t>(height);
        const size_t uv_size = static_cast<size_t>(chroma_width) * static_cast<size_t>(chroma_height);
        if (width <= 0 || height <= 0 || frame.data.size() < y_size + (uv_size * 2u)) {
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }
        const uint8_t* y_plane = frame.data.data();
        const uint8_t* u_plane = y_plane + y_size;
        const uint8_t* v_plane = u_plane + uv_size;
        NotePresentedSize(width, height);
        return video_renderer_.RenderI420ToSwapChain(
                   y_plane, width, u_plane, chroma_width, v_plane, chroma_width, width, height)
            ? OMNI_WEBRTC_MEDIA_STATUS_OK
            : OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }

    /// The size of the last frame put on the swapchain. The renderer fits it whole into the panel
    /// (FitViewport), so the shell needs its shape to know where on the panel the picture is — a
    /// remote-control viewer's pointer maps through it (MainWindow.ScreenControl.cs).
    void NotePresentedSize(int width, int height)
    {
        presented_width_.store(width, std::memory_order_relaxed);
        presented_height_.store(height, std::memory_order_relaxed);
    }

    void DetachVideoRenderer()
    {
        video_renderer_.DetachSwapChain();
        hardware_h264_decoder_.reset();
        hw_nv12_render_tex_.Reset();
        hw_nv12_width_ = 0;
        hw_nv12_height_ = 0;
        has_hw_nv12_frame_ = false;
    }

    // Task #84 increment 3: copy the HW decoder's NV12 output (a reused texture-array slice that may
    // lack SHADER_RESOURCE and be padding-aligned) into a stable, SRV-capable NV12 texture sized to
    // the visible frame, so RenderAttachedVideo can SRV + present it. Returns false on any failure
    // (caller falls back to SW decode). All on the renderer's device (one thread).
    // Read a hardware-decoded NV12 texture back and hand it to the software presentation path as
    // I420 — for a codec with no software decoder while no swapchain is attached.
    bool DeliverHwNv12AsI420(ID3D11Texture2D* src, UINT subresource, uint32_t rtp_timestamp)
    {
        ID3D11Device* device = video_renderer_.device();
        if (src == nullptr || device == nullptr) {
            return false;
        }
        D3D11_TEXTURE2D_DESC desc = {};
        src->GetDesc(&desc);
        const int width = std::min(std::max(2, width_), static_cast<int>(desc.Width)) & ~1;
        const int height = std::min(std::max(2, height_), static_cast<int>(desc.Height)) & ~1;
        D3D11_TEXTURE2D_DESC staging_desc = desc;
        staging_desc.ArraySize = 1;
        staging_desc.MipLevels = 1;
        staging_desc.Usage = D3D11_USAGE_STAGING;
        staging_desc.BindFlags = 0;
        staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
        staging_desc.MiscFlags = 0;
        Microsoft::WRL::ComPtr<ID3D11Texture2D> staging;
        if (FAILED(device->CreateTexture2D(&staging_desc, nullptr, &staging))) {
            return false;
        }
        Microsoft::WRL::ComPtr<ID3D11DeviceContext> context;
        device->GetImmediateContext(context.GetAddressOf());
        if (!context) {
            return false;
        }
        context->CopySubresourceRegion(staging.Get(), 0, 0, 0, 0, src, subresource, nullptr);
        D3D11_MAPPED_SUBRESOURCE mapped = {};
        if (FAILED(context->Map(staging.Get(), 0, D3D11_MAP_READ, 0, &mapped))) {
            return false;
        }
        const auto* y = static_cast<const uint8_t*>(mapped.pData);
        const uint8_t* uv = y + static_cast<size_t>(mapped.RowPitch) * desc.Height;
        webrtc::scoped_refptr<webrtc::I420Buffer> i420 = webrtc::I420Buffer::Create(width, height);
        libyuv::NV12ToI420(y, static_cast<int>(mapped.RowPitch), uv, static_cast<int>(mapped.RowPitch),
                           i420->MutableDataY(), i420->StrideY(), i420->MutableDataU(), i420->StrideU(),
                           i420->MutableDataV(), i420->StrideV(), width, height);
        context->Unmap(staging.Get(), 0);
        webrtc::VideoFrame frame = webrtc::VideoFrame::Builder()
                                       .set_video_frame_buffer(i420)
                                       .set_rtp_timestamp(rtp_timestamp)
                                       .build();
        (void)decode_callback_.Decoded(frame);
        return true;
    }

    bool StoreHwNv12Frame(ID3D11Texture2D* src, UINT subresource)
    {
        ID3D11Device* device = video_renderer_.device();
        if (src == nullptr || device == nullptr) {
            return false;
        }
        D3D11_TEXTURE2D_DESC src_desc = {};
        src->GetDesc(&src_desc);
        // Visible frame size = the session's decoded size, clamped to the (padding-aligned) source.
        const int copy_w = std::min(std::max(1, width_), static_cast<int>(src_desc.Width));
        const int copy_h = std::min(std::max(1, height_), static_cast<int>(src_desc.Height));
        if (!EnsureHwNv12RenderTexture(copy_w, copy_h)) {
            return false;
        }
        Microsoft::WRL::ComPtr<ID3D11DeviceContext> context;
        device->GetImmediateContext(context.GetAddressOf());
        if (!context) {
            return false;
        }
        D3D11_BOX box = {};
        box.left = 0;
        box.top = 0;
        box.front = 0;
        box.right = static_cast<UINT>(copy_w);
        box.bottom = static_cast<UINT>(copy_h);
        box.back = 1;
        context->CopySubresourceRegion(hw_nv12_render_tex_.Get(), 0, 0, 0, 0, src, subresource, &box);
        has_hw_nv12_frame_ = true;
        return true;
    }

    bool EnsureHwNv12RenderTexture(int width, int height)
    {
        if (width <= 0 || height <= 0) {
            return false;
        }
        if (hw_nv12_render_tex_ && hw_nv12_width_ == width && hw_nv12_height_ == height) {
            return true;
        }
        ID3D11Device* device = video_renderer_.device();
        if (device == nullptr) {
            return false;
        }
        D3D11_TEXTURE2D_DESC desc = {};
        desc.Width = static_cast<UINT>(width);
        desc.Height = static_cast<UINT>(height);
        desc.MipLevels = 1;
        desc.ArraySize = 1;
        desc.Format = DXGI_FORMAT_NV12;
        desc.SampleDesc.Count = 1;
        desc.Usage = D3D11_USAGE_DEFAULT;
        desc.BindFlags = D3D11_BIND_SHADER_RESOURCE;
        Microsoft::WRL::ComPtr<ID3D11Texture2D> texture;
        if (FAILED(device->CreateTexture2D(&desc, nullptr, texture.GetAddressOf()))) {
            return false;
        }
        hw_nv12_render_tex_ = texture;
        hw_nv12_width_ = width;
        hw_nv12_height_ = height;
        return true;
    }
#endif

    std::string DiagnosticsJson() const
    {
        std::lock_guard<std::mutex> lock(mutex_);
        std::ostringstream json;
        json
            << R"({"backend":"libwebrtc")"
            << R"(,"native_session_scaffold":true)"
            << R"(,"codec":")" << NativeVideoCodecName(codec_kind_) << '"'
#if defined(_WIN32)
            // «АППАРАТНЫЙ» СПРАШИВАЕТСЯ У САМОГО КОДИРОВЩИКА, А НЕ У НАЛИЧИЯ ОБЪЕКТА.
            //
            // С обёрткой отката (#61, шаг D2a) наличие аппаратного кодировщика и работа НА нём —
            // разные вещи: обёртка держит оба и переключается по факту отказа. Спрашивать «есть ли
            // объект» значило бы завести ещё одно имя, которое врёт ровно тогда, когда правда
            // важнее всего — после отката посреди звонка.
            // ДИАГНОСТИКА КОДИРОВЩИКА — ЧЕРЕЗ НАБЛЮДАТЕЛЯ, А НЕ ПО СЫРОМУ УКАЗАТЕЛЮ.
            //
            // Здесь читались внутренности MFT: имя, асинхронность, отказы смены скорости,
            // счётчики SPS/PPS/IDR. Указатель на аппаратный кодировщик стал ДИНАМИЧЕСКИМ:
            // кодировщиком владеет VideoStreamEncoder и пересоздаёт его при каждой перенастройке,
            // а перенастройка идёт на каждом такте ABR. Читать его снаружи — читать освобождённую
            // память, и это не рассуждение: замер поймал ACCESS_VIOLATION прямо в DiagnosticsJson
            // (стек снят 2026-08-25, крах в одном прогоне из пяти).
            //
            // Источник теперь тот же, которым пользуется апстрим: оркестратор сам сообщает, на
            // какой кодировщик он переключился (OnEncoderImplementationChanged). Часть прежних
            // чисел вместе с указателем ушла — они описывали внутренности пути, которым мы больше
            // не владеем; оставлять их значениями «на всякий случай» было бы враньём.
            << R"(,"encoder_backend":")"
            << (encoder_stats_observer_ && encoder_stats_observer_->implementation_is_hardware()
                    ? "mediafoundation_" + std::string(NativeVideoCodecName(codec_kind_)) + "_hw"
                    : std::string("libwebrtc")) << '"'
            << R"(,"hardware_encoder":)"
            << (encoder_stats_observer_ && encoder_stats_observer_->implementation_is_hardware()
                    ? "true"
                    : "false")
            << R"(,"encoder_implementation":")"
            << JsonEscape(encoder_stats_observer_
                              ? encoder_stats_observer_->implementation_name()
                              : std::string()) << '"'
            << R"(,"encoder_adaptation_changes":)"
            << (encoder_stats_observer_ ? encoder_stats_observer_->adaptation_changes() : 0)
            << R"(,"encoder_reconfigurations":)"
            << (encoder_stats_observer_ ? encoder_stats_observer_->reconfigurations() : 0)
            << R"(,"encoder_dropped_frames":)"
            << (encoder_stats_observer_ ? encoder_stats_observer_->dropped_frames() : 0)
            // Почему кодируем процессором. Пусто = аппаратный кодировщик поднялся. Без этой
            // строки откат был бы молчаливым, и «почему видео тормозит» пришлось бы гадать.
            << R"(,"encoder_fallback_reason":")"
            << JsonEscape(std::string_view(hardware_encoder_fallback_reason_)) << '"'
#else
            << R"(,"encoder_backend":"libwebrtc")"
            << R"(,"hardware_encoder":false)"
#endif
            << R"(,"payload_type":)" << static_cast<int>(NativeVideoCodecPayloadType(codec_kind_))
            << R"(,"width":)" << width_
            << R"(,"height":)" << height_
            << R"(,"fps":)" << fps_
            << R"(,"target_bitrate_bps":)" << bitrate_bps_
            << R"(,"negotiated_width":)" << negotiated_width_
            << R"(,"negotiated_height":)" << negotiated_height_
            << R"(,"negotiated_fps":)" << negotiated_fps_
            << R"(,"negotiated_bitrate_bps":)" << negotiated_bitrate_bps_
            << R"(,"abr_state":")" << AbrStateName(abr_state_) << '"'
            << R"(,"abr_reason":")" << JsonEscape(abr_reason_) << '"'
            << R"(,"abr_reason_history":)" << AbrReasonHistoryJsonUnsafe()
            << R"(,"abr_loss_ewma_q8":)" << static_cast<uint32_t>(std::max(0.0, std::round(abr_loss_ewma_q8_)))
            // Последний RR собеседника о нашем потоке (см. OmniReportBlockObserver).
            << R"(,"remote_reports":)" << sender_report_blocks_.reports()
            << R"(,"remote_fraction_lost_q8":)" << static_cast<uint32_t>(sender_report_blocks_.fraction_lost_q8())
            << R"(,"remote_jitter":)" << sender_report_blocks_.jitter()
            << R"(,"remote_rtt_ms":)" << sender_report_blocks_.rtt_ms()
            << R"(,"abr_rtt_ewma_ms":)" << static_cast<uint32_t>(std::max(0.0, std::round(abr_rtt_ewma_ms_)))
            << R"(,"abr_jitter_ewma":)" << static_cast<uint32_t>(std::max(0.0, std::round(abr_jitter_ewma_)))
            << R"(,"abr_target_width":)" << abr_target_width_
            << R"(,"abr_target_height":)" << abr_target_height_
            << R"(,"abr_target_fps":)" << abr_target_fps_
            << R"(,"abr_resolution_changes":)" << abr_resolution_changes_
            << R"(,"abr_target_bitrate_bps":)" << abr_target_bitrate_bps_
            // Wave 6 (2026-05-18): libwebrtc adaptation detector counters.
            // Each counter is monotonic; deltas across diagnostic ticks
            // show "events since last sample". Useful for verifying that
            // CPU/QP signals fire (or don't) under user-reported scenarios
            // — e.g. dark scene should NOT spike qp_high if the bandwidth
            // headroom gate works.
            // ШАГИ АДАПТАЦИИ СЧИТАЕТ ОРКЕСТРАТОР, А НЕ МЫ (#61, шаг D3).
            //
            // Здесь стояли adapt_cpu_up/down и adapt_qp_low/high — счётчики наших собственных
            // детекторов. Детекторов у сессии больше нет, и после переворота эти четыре числа
            // навсегда остались нулями, то есть врали: «детекторы молчат» вместо «мы их не
            // видим». Оркестратор считает то же самое, но ПО ОСЯМ РЕШЕНИЯ: сколько шагов
            // адаптации сделано из-за процессора и сколько из-за качества, отдельно по
            // разрешению и по частоте кадров.
            << R"(,"adapt_cpu_resolution_steps":)"
            << (encoder_stats_observer_ ? encoder_stats_observer_->cpu_resolution_steps() : 0)
            << R"(,"adapt_cpu_framerate_steps":)"
            << (encoder_stats_observer_ ? encoder_stats_observer_->cpu_framerate_steps() : 0)
            << R"(,"adapt_quality_resolution_steps":)"
            << (encoder_stats_observer_ ? encoder_stats_observer_->quality_resolution_steps() : 0)
            << R"(,"adapt_quality_framerate_steps":)"
            << (encoder_stats_observer_ ? encoder_stats_observer_->quality_framerate_steps() : 0)
            // Само пожелание оркестратора источнику — то, из чего складывается цель ABR по
            // размеру и частоте. Без него «размер не поменялся» одинаково читается и как
            // «адаптация не сработала», и как «сработала, но мы её не применили».
            << R"(,"degradation_preference":")"
            << (screen_content_ ? "maintain_resolution" : "balanced") << '"'
            //
            // Пожелания печатаются ЗАЖАТЫМИ выбором отправителя. «Ограничения нет» у апстрима
            // выражается максимумом int, и печатать 2147483647 значило бы предложить человеку
            // вычитать это в уме каждый раз. Зажатое число — ровно то, что и управляет целью.
            << R"(,"encoder_wants_max_pixels":)"
            << std::min<int64_t>(
                   push_video_source_
                       ? static_cast<int64_t>(push_video_source_->wants().max_pixel_count)
                       : 0,
                   static_cast<int64_t>(negotiated_width_) * negotiated_height_)
            << R"(,"encoder_wants_max_fps":)"
            << std::min<int64_t>(
                   push_video_source_
                       ? static_cast<int64_t>(push_video_source_->wants().max_framerate_fps)
                       : 0,
                   negotiated_fps_)
            << R"(,"encoder_wants_updates":)"
            << (push_video_source_ ? push_video_source_->wants_updates() : 0)
            // Вход сканера полосы: габариты последнего закодированного кадра и фактический
            // выход за то же пятисекундное окно, по которому судит он сам.
            // ДОХОДИТ ЛИ ЦЕЛЬ ДО КОДИРОВЩИКА И КУДА ДЕВАЮТСЯ КАДРЫ — ДВА ЧИСЛА, КОТОРЫЕ
            // РАЗЛИЧАЮТ ПОЛОМКИ, НЕРАЗЛИЧИМЫЕ СНАРУЖИ.
            //
            // Живой звонок 2026-08-27: кадры входят (~33 в секунду), наружу не выходит ничего,
            // на провод не уходит ни пакета. «Кодировщик остался без цели и молча выбрасывает»
            // и «кадры теряются раньше кодировщика» выглядят одинаково, а чинятся по-разному
            // (docs/bugs/encoder-goes-silent-mid-call.md).
            //
            // Первое число — сколько раз цель регулятора реально дошла до оркестратора и какой
            // она была в последний раз (-1 = не доходила ни разу). Второе — сколько кадров
            // оркестратор ПРИНЯЛ, сколько закодировал и сколько выбросил, с разбивкой по причине,
            // как её называет он сам.
            // Вход и выход САМОГО кодировщика: сколько раз позван, сколько раз вернулся, сколько
            // вернулся пустым (аппаратный конвейер имеет право), и сколько занял самый долгий
            // вызов. Encode синхронный и идёт с очереди кодирования — долгий вызов и есть та
            // занятая очередь, из-за которой кадры выбрасываются как kEncoderQueue.
            << R"(,"mft_encode_calls":)"
            << (encoder_io_stats_ ? encoder_io_stats_->encode_calls.load() : 0)
            << R"(,"mft_encode_returns":)"
            << (encoder_io_stats_ ? encoder_io_stats_->encode_returns.load() : 0)
            << R"(,"mft_encode_ok":)"
            << (encoder_io_stats_ ? encoder_io_stats_->encode_ok.load() : 0)
            << R"(,"mft_encode_empty":)"
            << (encoder_io_stats_ ? encoder_io_stats_->encode_empty.load() : 0)
            << R"(,"mft_encode_fallback_requests":)"
            << (encoder_io_stats_ ? encoder_io_stats_->fallback_requests.load() : 0)
            << R"(,"mft_encode_errors":)"
            << (encoder_io_stats_ ? encoder_io_stats_->encode_errors.load() : 0)
            << R"(,"mft_encode_max_ms":)"
            << (encoder_io_stats_ ? encoder_io_stats_->max_encode_ms.load() : 0)
            << R"(,"mft_encode_last_ms":)"
            << (encoder_io_stats_ ? encoder_io_stats_->last_encode_ms.load() : 0)
            // Среднее время вызова — суммой, делённой на число вызовов. Именно это число
            // отвечает на вопрос «кодировщик медленный или его редко зовут».
            << R"(,"mft_encode_avg_ms":)"
            << ((encoder_io_stats_ && encoder_io_stats_->encode_returns.load() > 0)
                    ? encoder_io_stats_->total_encode_ms.load() /
                          static_cast<int64_t>(encoder_io_stats_->encode_returns.load())
                    : 0)
            << R"(,"mft_last_error":")"
            << JsonEscape(EncoderLastError()) << '"'
            << R"(,"encoder_target_updates":)" << encoder_target_updates_
            << R"(,"encoder_last_target_bps":)" << encoder_last_target_bps_
            << R"(,"encoder_incoming_frames":)"
            << (encoder_stats_observer_ ? encoder_stats_observer_->incoming_frames() : 0)
            << R"(,"encoder_encoded_frames":)"
            << (encoder_stats_observer_ ? encoder_stats_observer_->encoded_frames() : 0)
            << R"(,"encoder_dropped_frames":)"
            << (encoder_stats_observer_ ? encoder_stats_observer_->dropped_frames() : 0)
            << R"(,"encoder_dropped_by_queue":)"
            << (encoder_stats_observer_ ? encoder_stats_observer_->dropped_by_queue() : 0)
            << R"(,"encoder_dropped_by_encoder":)"
            << (encoder_stats_observer_ ? encoder_stats_observer_->dropped_by_encoder() : 0)
            << R"(,"encoder_dropped_by_rate":)"
            << (encoder_stats_observer_ ? encoder_stats_observer_->dropped_by_rate() : 0)
            << R"(,"encoded_frame_width":)" << encoded_width_.load(std::memory_order_relaxed)
            << R"(,"encoded_frame_height":)" << encoded_height_.load(std::memory_order_relaxed)
            << R"(,"encoded_frame_qp":)" << encoded_qp_.load(std::memory_order_relaxed)
            << R"(,"encoded_window_bps":)" << EncodedWindowBps()
            << R"(,"adapt_scaling_enabled":")"
            << (encoder_stats_observer_
                    ? (std::string(encoder_stats_observer_->quality_resolution_scaling() ? "q-res " : "")
                       + (encoder_stats_observer_->quality_framerate_scaling() ? "q-fps " : "")
                       + (encoder_stats_observer_->cpu_resolution_scaling() ? "cpu-res" : ""))
                    : std::string("?")) << '"'
            << R"(,"adapt_settings_updates":)"
            << (encoder_stats_observer_ ? encoder_stats_observer_->adaptation_settings_updates() : 0)
            << R"(,"encoder_wants_narrowed":)"
            << (push_video_source_ ? push_video_source_->wants_narrowed() : 0)
            << R"(,"encoder_wants_widened":)"
            << (push_video_source_ ? push_video_source_->wants_widened() : 0)
            << R"(,"adapt_overuse_ticks":)" << (cpu_overuse_metrics_observer_ ? cpu_overuse_metrics_observer_->ticks() : 0)
            // Отдано в ядро / выпущено без получателя (см. OmniPacketSink). Очереди, глубину
            // которой здесь показывали, больше нет.
            << R"(,"packets_handed_to_core":)" << packet_sink_.handed()
            << R"(,"packets_unrouted":)" << packet_sink_.unrouted()
            << R"(,"frames_encoded":)" << frames_encoded_
            << R"(,"frames_decoded":)" << frames_decoded_
#if defined(_WIN32)
            // hw_frames_decoded > 0 IS the reliable "receiver is on the HW DXVA decode path"
            // signal (each increment = one NV12 texture produced + copied). A separate
            // hw_decoder_active pointer-presence field proved unreliable across the diag/decode
            // thread boundary and was dropped — count, not a flag.
            << R"(,"hw_frames_decoded":)" << hw_frames_decoded_
            << R"(,"hw_decode_failures":)" << hw_decode_failures_
#endif
            << R"(,"frames_drained":)" << frames_drained_
            << R"(,"rtp_packets_sent":)" << rtp_packets_sent_.load(std::memory_order_relaxed)
            << R"(,"rtp_octets_sent":)" << rtp_octets_sent_.load(std::memory_order_relaxed)
            << R"(,"rtp_packets_received":)" << rtp_packets_received_
            << R"(,"decode_drops":)" << decode_callback_.dropped_frames()
            << R"(,"encoded_callback_drops":)" << encode_callback_.dropped_frames()
            << R"(,"rate_updates":)" << rate_updates_
            << R"(,"keyframe_requests":)" << keyframe_requests_
            // Wave 5b (2026-05-18): receiver_missing_packets_dropped_at_cap
            // removed — NackRequester handles burst recovery internally and
            // escalates to KeyFrameRequestSender when its retry budget is
            // exhausted. We do not surface its internal "saturated" state.
            << R"(,"encode_failures":)" << encode_failures_
            << R"(,"frames_skipped_by_encoder":)" << frames_skipped_by_encoder_
            << R"(,"frames_reference":)" << frames_reference_
            << R"(,"frames_non_reference":)" << frames_non_reference_
            // ВЕСЬ ПУТЬ ОТЧЁТА О ДОСТАВКЕ, СВЕРХУ ВНИЗ: построен генератором -> отдан оболочке ->
            // вытеснен из буфера -> применён отправителем. Ноль на любом шаге теперь называет
            // виновника сам, без гадания по соседним числам.
            << R"(,"feedback_generator_ready":)" << (feedback_generator_ ? 1 : 0)
            << R"(,"transport_feedback_reports_built":)"
            << transport_feedback_reports_built_.load(std::memory_order_relaxed)
            << R"(,"transport_feedback_reports_applied":)" << transport_feedback_reports_applied_
            // «СОПОСТАВЛЕНО» СТАЛО «ДАЛО ОЦЕНКУ» (#57, шаг C2b).
            //
            // Раньше это число считал наш адаптер: сколько отчётов нашли свои пакеты в нашей
            // истории отправки. Истории у нас больше нет — её ведёт сборщик. А спрашивать
            // надо было с самого начала не «сопоставилось ли», а «изменилась ли оценка»:
            // сопоставление — не результат, и ровно на этой подмене держалась зелёная и
            // неверная картина 2026-08-18 (239 отчётов из 239 сопоставлены, оценок ноль).
            // Теперь это число обновлений цели от регулятора сборщика.
            << R"(,"transport_feedback_reports_matched":)"
            << (target_rate_observer_ ? target_rate_observer_->updates() : 0)
            << R"(,"transport_feedback_packets_reported":)" << transport_feedback_packets_reported_
            << R"(,"goog_cc_feedback_target_bps":)" << goog_cc_feedback_target_bps_
            << R"(,"transport_rtt_us":)" << transport_rtt_us_
            // ПОЛЕ probe_clusters_created СНЯТО (#57, шаг C2b). Пробы заказывает и ведёт
            // сборщик, наружу их число он не отдаёт, а печатать вечный ноль под именем,
            // которое раньше значило «пробинг жив», — худший из вариантов: ровно так
            // читается «пробинга нет вовсе», из-за чего этот счётчик и заводился.
            << R"(,"rtp_start_timestamp":)"
            << rtp_start_timestamp_.load(std::memory_order_relaxed)
            << R"(,"transport_controller_created":)"
            << (transport_controller_created_.load(std::memory_order_relaxed) ? 1 : 0)
            << R"(,"controller_target_bps":)"
            << (target_rate_observer_ ? target_rate_observer_->target_bps() : 0)
            << R"(,"controller_target_updates":)"
            << (target_rate_observer_ ? target_rate_observer_->updates() : 0)
            << R"(,"module_created":)"
            << (module_created_.load(std::memory_order_relaxed) ? 1 : 0)
            << R"(,"module_rtp_sent":)" << module_rtp_sent_.load(std::memory_order_relaxed)
            << R"(,"module_send_failures":)" << module_send_failures_.load(std::memory_order_relaxed)
            << R"(,"module_retransmissions_sent":)"
            << module_retransmissions_sent_.load(std::memory_order_relaxed)
            << R"(,"module_padding_sent":)" << module_padding_sent_.load(std::memory_order_relaxed)
            << R"(,"receiver_rtcp_ready":)"
            << (receiver_rtcp_ready_.load(std::memory_order_relaxed) ? 1 : 0)
            // RTCP: отдано в ядро / принято. Очереди между ними больше нет.
            << R"(,"rtcp_packets_sent":)"
            << rtcp_packets_sent_.load(std::memory_order_relaxed)
            << R"(,"rtcp_packets_received":)"
            << rtcp_packets_received_.load(std::memory_order_relaxed)
            << R"(,"probe_without_media_allowed":)"
            << (probe_without_media_allowed_.load(std::memory_order_relaxed) ? 1 : 0)
            << R"(,"received_generic_descriptors":)" << received_generic_descriptors_
            << R"(,"received_generic_dependencies":)" << received_generic_dependencies_
            << R"(,"rtx_packets_received":)" << rtx_packets_received_
            << R"(,"rtx_packets_recovered":)" << rtx_packets_recovered_
            << R"(,"decode_failures":)" << decode_failures_
            << R"(,"last_decode_status":)" << last_decode_status_
            << R"(,"last_failed_frame_head":")" << JsonEscape(last_failed_frame_head_) << '"' 
            << R"(,"h264_decoder_status_failures":)" << h264_decoder_status_failures_
            << R"(,"h264_sps_pps_tracker_drops":)" << h264_sps_pps_tracker_drops_
            << R"(,"h264_sps_pps_tracker_keyframe_requests":)" << h264_sps_pps_tracker_keyframe_requests_
            << R"(,"h264_annexb_rebuilds":)" << h264_annexb_rebuilds_
            << R"(,"h264_annexb_rebuild_failures":)" << h264_annexb_rebuild_failures_
            << R"(,"h264_receiver_idr_frames":)" << h264_receiver_idr_frames_
            << R"(,"h264_receiver_sps_frames":)" << h264_receiver_sps_frames_
            << R"(,"h264_receiver_pps_frames":)" << h264_receiver_pps_frames_
            // ИМЯ ТЕПЕРЬ СОВПАДАЕТ СО ЗНАЧЕНИЕМ: это ВЫХОД СБОРЩИКА КАДРОВ.
            //
            // До #60 счётчик рос только у кадров, которые приняли ЕЩЁ И в буфер показа, то
            // есть складывал две разные вещи и назывался при этом именем одной из них. Ровно
            // на этом два дня искали «обвал сборки», которого не было. Всё, что относится к
            // показу, спрашивается ниже у самого контроллера и никогда больше — здесь.
            << R"(,"rtp_frames_assembled":)" << rtp_frames_assembled_
            << R"(,"no_assembled_watchdog_fired":)" << no_assembled_watchdog_fired_
            << R"(,"max_assembly_gap_ms":)" << max_assembly_gap_ms_
            // ПОКАЗ ОТЧИТЫВАЕТСЯ ЧЕРЕЗ АПСТРИМОВСКОГО НАБЛЮДАТЕЛЯ (#60).
            //
            // Прежние поля (playout_insert_rejected_full/redundant, playout_drops,
            // playout_undecodable_polls, playout_wait_polls) описывали устройство НАШЕЙ обвязки,
            // которой больше нет. Взамен — то, что о себе рассказывает сам контроллер:
            //   playout_continuity_advances — сколько вставок продвинуло непрерывную цепочку
            //     (НЕ число кадров: одна вставка может сделать непрерывными сразу несколько);
            //   playout_frames_dropped   — сколько выбросил (ёмкость, недекодируемость);
            //   playout_target_delay_ms  — целевая задержка показа, посчитанная VCMTiming;
            //   playout_minimum_delay_ms — её нижняя граница от оценки джиттера;
            //   playout_jitter_delay_ms  — сколько кадр реально пролежал в буфере;
            //   playout_decode_time_ms   — 95-й процентиль времени декодирования.
            << R"(,"playout_pending_frames":)"
            << playout_pending_frames_.load(std::memory_order_relaxed)
            << R"(,"playout_continuity_advances":)"
            << (playout_stats_ ? playout_stats_->continuity_advances() : 0)
            << R"(,"playout_keyframes":)"
            << (playout_stats_ ? playout_stats_->keyframes() : 0)
            << R"(,"playout_frames_dropped":)"
            << (playout_stats_ ? playout_stats_->dropped_frames() : 0)
            << R"(,"playout_target_delay_ms":)"
            << (playout_stats_ ? playout_stats_->target_delay_ms() : 0)
            << R"(,"playout_minimum_delay_ms":)"
            << (playout_stats_ ? playout_stats_->minimum_delay_ms() : 0)
            << R"(,"playout_jitter_delay_ms":)"
            << (playout_stats_ ? playout_stats_->jitter_buffer_delay_ms() : 0)
            << R"(,"playout_jitter_delay_max_ms":)"
            << (playout_stats_ ? playout_stats_->jitter_buffer_delay_max_ms() : 0)
            << R"(,"playout_current_delay_ms":)"
            << (playout_stats_ ? playout_stats_->current_delay_ms() : 0)
            << R"(,"playout_decode_time_ms":)"
            << (playout_stats_ ? playout_stats_->estimated_decode_time_ms() : 0)
            << R"(,"playout_frames_awaiting_keyframe":)"
            << playout_frames_awaiting_keyframe_.load(std::memory_order_relaxed)
            << R"(,"decode_timeouts":)" << decode_timeouts_.load(std::memory_order_relaxed)
            << R"(,"receiver_highest_sequence":)" << HighestExtendedSequenceUnsafe()
            << R"(,"receiver_jitter":)" << ReceiverStatsUnsafe().jitter
            << R"(,"receiver_cumulative_lost":)" << ReceiverStatsUnsafe().packets_lost
            << R"(,"receiver_packets":)" << ReceiverStatsUnsafe().packet_counter.packets
            << R"(,"receiver_out_of_order_packets":)" << receiver_out_of_order_packets_
            << R"(,"presented_width":)" << presented_width_.load(std::memory_order_relaxed)
            << R"(,"presented_height":)" << presented_height_.load(std::memory_order_relaxed)
            << BweDiagnosticsJsonFields()
            // Просьбы приёмника — как их посчитал сам модуль на проводе (пакеты NACK / номера в них
            // / PLI), и как их принял отправитель. Плюс почему последний раз просили кадр.
            << R"(,"rtcp_nack_sent":)" << receiver_rtcp_sent_.nack_packets()
            << R"(,"rtcp_nack_seq_sent":)" << receiver_rtcp_sent_.nack_requests()
            << R"(,"rtcp_pli_sent":)" << receiver_rtcp_sent_.pli_packets()
            << R"(,"rtcp_nack_received":)" << sender_rtcp_received_.nack_packets()
            << R"(,"rtcp_nack_seq_received":)" << sender_rtcp_received_.nack_requests()
            << R"(,"rtcp_pli_received":)" << sender_rtcp_received_.pli_packets()
            << R"(,"last_keyframe_request_reason":")" << KeyframeReasonName(last_keyframe_request_reason_) << '"'
            << R"(,"receiver_rtt_ms":)" << receiver_rtt_ms_.load(std::memory_order_relaxed)
            << R"(,"decoded_pending_frames":)" << decode_callback_.queued_frames()
            << '}';
        return json.str();
    }

private:
    static constexpr size_t kRtpHeaderBytes = 12;
    static constexpr int kRtpClockHz = 90'000;
    // ПРЕДЕЛ ПАКЕТА ПРИХОДИТ ИЗ СОГЛАСОВАНИЯ, А НЕ ИЗ ЭТОГО ФАЙЛА.
    //
    // Здесь стояла константа 1000 — третья копия одного и того же числа. Первые две живут там,
    // где им и место: у транспорта (`REALTIME_MEDIA_DATAGRAM_MAX_BYTES` — потолок, который QUIC
    // реально пропустит) и в согласовании сессии, где стороны берут минимум из своих бюджетов
    // (`select_realtime_media_configuration`). Медиа-слой про канал не знает ничего и права
    // назначать его предел не имеет: сегодня совпало, завтра транспорт поднимет потолок, и
    // третья копия молча останется старой.
    //
    // Значение приезжает ключом `max_datagram_bytes` в конфигурации сессии. Ниже — только
    // умолчание на случай, когда ключа нет: стенды, которым канал не важен.
    static constexpr size_t kDefaultMaxRtpPacketBytes = 1000;
    // Границы вменяемости. Снизу — чтобы заголовок RTP и полезная нагрузка вообще поместились;
    // сверху — потолок транспорта: датаграмму крупнее он отвергнет, и кадр умрёт целиком.
    static constexpr size_t kMinAllowedRtpPacketBytes = 300;
    static constexpr size_t kMaxAllowedRtpPacketBytes = 1200;
    // Пределы своего кэша отправленного убраны вместе с ним: глубину истории повторов теперь
    // задаёт kSendSidePacketHistorySize у модуля.
    static constexpr size_t kMaxReceivedRtpPayloadCachePackets = 4096;
    static constexpr size_t kMaxReceivedRtpPayloadCacheBytes = 4u * 1024u * 1024u;
    // Wave 5 Phase 5a-step2: pacing constants gone — PacingController owns
    // the rate policy.
    // ЗАДЕРЖКА ПОКАЗА НЕ ЗАДАЁТСЯ ЗДЕСЬ ВООБЩЕ (#60). Её считает VCMTiming внутри
    // VideoStreamBufferController — из оценки джиттера, времени декодирования и задержки показа.
    // Наши kPlayoutDelayMs, kMaxPlayoutBytes, kMaxPlayoutFrames и kMaxPlayoutFrameAgeMs ушли
    // вместе с обвязкой, которая их читала.
    // Wave 5b (2026-05-18): kMaxMissingTracked / kNackIntervalMs / kNackReorderGraceMs /
    // kNackRetryIntervalMs / kMissingPacketExpiryMs / kMaxNackAttemptsPerPacket removed.
    // NackRequester (libwebrtc) owns these knobs internally — its internal
    // defaults: kMaxNackPackets=1000, kMaxNackRetries=100, kMaxPacketAge=10s.
    // See modules/video_coding/nack_requester.h and the Wave 5b ADR.
    // kRetransmissionCooldownMs (80 мс) убран вместе со своим кэшем: слишком частый повтор теперь
    // отбивает RtpPacketHistory, и делает это точнее — по времени последней отправки КАЖДОГО
    // пакета, а не по одному числу на всех (RTPSender::OnReceivedNack -> SetRtt(5 + rtt)).
    static constexpr int64_t kKeyframeRequestIntervalMs = 1000;
    // Срок годности оценки GoogCC. Регулятор отдаёт цель только при её ИЗМЕНЕНИИ, поэтому
    // «в этот раз ничего не сказал» значит «мнение прежнее», а не «мнения нет» — но лишь
    // пока его подпитывают. Окно на несколько тактов ABR (такт — около секунды).
    static constexpr int64_t kGoogCcTargetMemoryMs = 4000;
    static constexpr uint32_t kReasonMissingRtpPackets = 1;
    static constexpr uint32_t kReasonDecodeFailure = 2;
    // 3 (kReasonPlayoutExpired) СНЯТ ВМЕСТЕ С ПРИЧИНОЙ (#60). Кадр выбрасывался по возрасту
    // нашей собственной DecodeDuePlayoutFrames — и, выбрасывая опорный кадр, она рвала цепочку
    // ссылок насовсем. Апстримовский буфер показа так не делает: он планирует показ по
    // VCMTiming, а не выбрасывает то, что уже можно декодировать.
    // Wave 5c (2026-05-18): kReasonPlayoutBudgetExceeded (4) removed.
    // FrameBuffer has no byte-budget concept — it caps by frame count.
    // Capacity overflow there silently drops the oldest unit; no keyframe
    // is requested since dropping a delta frame doesn't require it.
    // Wave 5b (2026-05-18): NackRequester exhausted its retry budget and
    // wants a full keyframe instead of further NACKs. Distinct from
    // kReasonMissingRtpPackets so we can track in diagnostics how often
    // this escalation fires vs. other keyframe causes.
    static constexpr uint32_t kReasonNackEscalation = 5;
    // A decoder that just joined the stream has no reference frame at all. Distinct from a
    // decode failure so diagnostics can tell "we started" from "we broke".
    static constexpr uint32_t kReasonDecoderStarted = 6;
    // Packets keep arriving but nothing assembles into a frame. The receiver's blind spot: with
    // no assembled frame there is no decode to fail and no playout to expire, so NOT ONE of the
    // triggers above fires and the picture freezes forever while the sender happily encodes.
    // Measured at 10% uniform loss: decoded stuck at 12 frames for a full minute, pliSent=1.
    // See docs/bugs/video-dies-on-packet-loss.md.
    static constexpr uint32_t kReasonNoAssembledFrames = 7;
    // НИЧЕГО НЕ ДЕКОДИРУЕТСЯ, ХОТЯ КАДРЫ СОБИРАЮТСЯ И ЛОЖАТСЯ В БУФЕР.
    //
    // Отдельная причина от kReasonNoAssembledFrames, и разница здесь не терминологическая: тот
    // сторож смотрит на СБОРКУ, а ломается ДЕКОДИРУЕМОСТЬ. Замер при 10% потерь: собрано 90 из
    // 90, в буфере лежит 77, декодировано 7, сторож сборки не срабатывал ни разу — потому что
    // сборке нечего было ловить. Один безвозвратно потерянный кадр рвёт цепочку зависимостей, и
    // всё, что пришло после, недекодируемо НАВСЕГДА: у нас линейная цепочка ссылок и ровно один
    // ключевой кадр в начале. Лечится только новым ключевым кадром — а просить его было некому.
    static constexpr uint32_t kReasonDecodeTimeout = 8;

    static const char* KeyframeReasonName(uint32_t reason_code)
    {
        switch (reason_code) {
        case kReasonMissingRtpPackets: return "missing_rtp_packets";
        case kReasonDecodeFailure: return "decode_failure";
        case kReasonNackEscalation: return "nack_escalation";
        case kReasonDecoderStarted: return "decoder_started";
        case kReasonNoAssembledFrames: return "no_assembled_frames";
        case kReasonDecodeTimeout: return "decode_timeout";
        default: return "none";
        }
    }
    /// Сколько меток времени «этот кадр достраивался повтором» держим. При 30 кадрах в секунду
    /// это около шестнадцати секунд истории — заведомо больше любого круга ретрансмиссии.
    static constexpr size_t kMaxRetransmittedTimestamps = 512;
    // Сроки взяты у апстрима (video/video_receive_stream2.cc): kMaxWaitForKeyFrame = 200 мс,
    // kMaxWaitForFrame = 3 с. Первый действует, пока ждём ключевой кадр (уже попросили — значит
    // переспрашивать надо быстро), второй — обычная терпимость к паузе в потоке.
    static constexpr int64_t kMaxWaitForKeyframeMs = 200;
    static constexpr int64_t kMaxWaitForFrameMs = 3000;
    // How long "packets in, frames out = 0" is tolerated before asking for a keyframe. Two seconds
    // is several frame intervals at any supported rate, so a merely-slow assembly (reordering, a
    // NACK round-trip) is not mistaken for a broken stream. The request itself is throttled to
    // `kKeyframeRequestIntervalMs`, so a stream that stays broken re-asks once a second — which is
    // the point: the old behaviour was to stop asking entirely.
    static constexpr int64_t kNoAssembledFrameTimeoutMs = 2'000;
    // 150 kbps minimum — see docs/decisions/abr-floor-correction.md.
    // Previously 1'500'000 (typo); clamped controller above viable rates for
    // degraded networks. Aligned with WebRTC standard video minimum (~150 kbps)
    // and consistent with the 64 kbps `set_rates` floor below.
    static constexpr int kAbrMinBitrateBps = 150'000;
    // Пол частоты кадров и пауза перед подъёмом разрешения сняты вместе с нашей политикой
    // (#61, шаг D3). Паузу между шагами адаптации держит VideoStreamAdapter, а пола частоты у
    // апстрима нет вовсе — точнее, он равен двум кадрам, и это правильно: под совсем плохой канал
    // редкая, но чёткая картинка полезнее частой каши.
    // Пороги подтверждения затора (kAbrConfirmedCongestionSamples / kAbrSevereCongestionSamples /
    // kAbrRecoveryStableSamples) и порог молчания обратной связи (kAbrFeedbackLostMs) сняты вместе
    // с самописной лестницей. Их работу делает GoogCC: подтверждение тренда — в
    // SendSideBandwidthEstimation, а «обратная связь замолчала» — в RttBasedBackoff, где молчание
    // само превращается в растущий RTT (CorrectedRtt = время с последнего отчёта + последний RTT),
    // и при переходе за 3 с оценка режется на 80% раз в секунду.
    // Отчёты пира приходят примерно раз в секунду, поэтому два пропущенных — первый момент,
    // когда молчание уже факт, а не джиттер. Нарочно короче таймеров живости: это решает,
    // насколько мы громкие, а не существует ли собеседник, и лишняя секунда тишины стоит
    // несравнимо меньше, чем лишняя секунда затопленного аплинка.
    static constexpr uint32_t kAbrFeedbackLostMs = 2'000;
    static constexpr size_t kAbrReasonHistoryLimit = 8;

    // Своей структуры кадра показа здесь нет и не должно быть: хранит кадры FrameBuffer внутри
    // контроллера, а время прибытия едет в самом кадре (OmniAssembledFrame) — там, где его и ждёт
    // апстрим.

    // Wave 5b (2026-05-18): MissingPacketState struct removed; NackRequester
    // tracks {seq_num, send_at_seq_num, created_at_time, sent_at_time,
    // retries} internally in its own NackInfo.

    LibWebRtcVideoSession() = default;

    bool Initialize(std::string_view config_json, std::string& error)
    {
        std::optional<NativeVideoCodec> codec_kind =
            ParseNativeVideoCodec(FindJsonString(config_json, "codec").value_or("h264"));
        if (!codec_kind.has_value()) {
            error = "native libwebrtc session supports H.264 only";
            return false;
        }

        codec_kind_ = *codec_kind;
        assembler_ = std::make_unique<webrtc::RtpVideoFrameAssembler>(
            NativeVideoAssemblerFormat(codec_kind_));
        // Разбор RTX. Ставится ВСЕГДА, даже пока отправитель RTX не шлёт: приёмная сторона должна
        // уметь принять то, что ей однажды пришлют, а не начинать уметь одновременно с этим.
        //
        // Внутри RtxReceiveStream есть SequenceChecker на карту типов нагрузки. Карта задаётся
        // здесь один раз и больше не меняется, а в Release проверка вырождается в пустышку
        // (SequenceCheckerDoNothing) — то есть построение вне приёмного потока безопасно. Написано
        // не для оправдания, а чтобы следующий читатель не искал здесь гонку, которой нет.
        rtx_media_sink_ = std::make_unique<OmniRtxMediaSink>(this);
        rtx_receive_stream_ = std::make_unique<webrtc::RtxReceiveStream>(
            env_,
            rtx_media_sink_.get(),
            std::map<int, int>{{kOmniVideoRtxPayloadType,
                                NativeVideoCodecPayloadType(codec_kind_)}},
            kOmniVideoSsrc);
        width_ = ClampInt(FindJsonInt(config_json, "width").value_or(640), 16, 3840);
        height_ = ClampInt(FindJsonInt(config_json, "height").value_or(360), 16, 2160);
        fps_ = ClampInt(FindJsonInt(config_json, "fps").value_or(30), 1, 120);
        bitrate_bps_ = ClampInt(FindJsonInt(config_json, "bitrate_bps").value_or(800'000), 64'000, 20'000'000);
        // Device selection (task #87): PCI vendor of the GPU the user picked for each direction
        // (0 = Auto). Encode honours the vendor via the encoder MFT filter; decode honours it via
        // the renderer's adapter choice (the HW decoder binds to that device). decode_software=1
        // skips the HW decoder entirely (libwebrtc SW decode).
        //
        // encode_software=1 — симметричный ключ, теперь он ЕСТЬ. Раньше его намеренно не
        // разбирали: «production H.264 encode is HW-only for now». Это перестало быть верным
        // 2026-08-15 — кодирование процессором стало полноправным путём, а не отсутствием пути:
        // без аппаратного кодировщика видео не работало вовсе.
        // См. docs/decisions/device-codec-selection.md и docs/bugs/hardware-h264-never-started.md.
        preferred_encode_vendor_id_ = static_cast<uint32_t>(
            std::max(0, FindJsonInt(config_json, "encode_vendor_id").value_or(0)));
        preferred_decode_vendor_id_ = static_cast<uint32_t>(
            std::max(0, FindJsonInt(config_json, "decode_vendor_id").value_or(0)));
        force_software_decode_ = FindJsonInt(config_json, "decode_software").value_or(0) != 0;
        force_software_encode_ = FindJsonInt(config_json, "encode_software").value_or(0) != 0;
        // ПРЕДЕЛ ДАТАГРАММЫ — СОГЛАСОВАННЫЙ, А НЕ НАШ. См. kDefaultMaxRtpPacketBytes.
        max_rtp_packet_bytes_ = static_cast<size_t>(ClampInt(
            FindJsonInt(config_json, "max_datagram_bytes")
                .value_or(static_cast<int>(kDefaultMaxRtpPacketBytes)),
            static_cast<int>(kMinAllowedRtpPacketBytes),
            static_cast<int>(kMaxAllowedRtpPacketBytes)));
        // Только для стенда: заставить аппаратный кодировщик отказывать после N удачных кадров.
        // См. большой комментарий в OmniHardwareH264Encoder::Encode.
        test_hw_encode_fail_after_ =
            std::max(0, FindJsonInt(config_json, "test_hw_encode_fail_after").value_or(0));
        // ДЕМОНСТРАЦИЯ ЭКРАНА — ДРУГОЙ РАЗМЕН, И РЕШАЕТ ЕГО ПРИЛОЖЕНИЕ (#61, шаг D3).
        //
        // У камеры под нехваткой полосы правильно шагать И размером, И частотой: лицо остаётся
        // узнаваемым в мелком кадре. У экрана — нет: уменьшенный текст нечитаем, и человек теряет
        // ровно то, ради чего включил демонстрацию. Поэтому апстрим для экрана запрещает адаптацию
        // по качеству вовсе (webrtc_video_engine.cc:2424, `!is_screencast`) и платит частотой
        // кадров, а не точками.
        //
        // Без этого ключа шаг D3 сделал бы демонстрацию экрана ХУЖЕ, чем она была: замер стенда
        // rampup показывает спуск до 990x556 при 918 кбит/с — по таблице для недостоверного QP
        // 1280x720 «стоит» 1500 кбит/с, и оркестратор совершенно правильно решает, что для камеры
        // такой кадр не обеспечен. Для экрана это правильное решение неверной задачи.
        screen_content_ = FindJsonInt(config_json, "content_screen").value_or(0) != 0;
        negotiated_width_ = std::max(
            width_,
            ClampInt(FindJsonInt(config_json, "negotiated_width").value_or(width_), 16, 3840));
        negotiated_height_ = std::max(
            height_,
            ClampInt(FindJsonInt(config_json, "negotiated_height").value_or(height_), 16, 2160));
        negotiated_fps_ = std::max(
            fps_,
            ClampInt(FindJsonInt(config_json, "negotiated_fps").value_or(fps_), 1, 120));
        negotiated_bitrate_bps_ = std::max(
            bitrate_bps_,
            ClampInt(
                FindJsonInt(config_json, "negotiated_bitrate_bps").value_or(bitrate_bps_),
                64'000,
                20'000'000));
        abr_target_width_ = width_;
        abr_target_height_ = height_;
        abr_target_fps_ = fps_;
        abr_target_bitrate_bps_ = bitrate_bps_;

        // Wave 6 (2026-05-18): adaptation primitives (CPU + QP).
        // Failure here is non-fatal: if for some reason the task queue can't
        // be created we still want media to work; we just lose the CPU/QP
        // contributions to ABR and rely on the network-only path.
        //
        // Task-queue contract: OveruseFrameDetector binds to the task queue
        // we hand to StartCheckForOveruse. QualityScaler is more delicate —
        // its constructor calls TaskQueueBase::Current()->PostDelayedTask to
        // start its CheckQpTask, so it MUST be constructed on the task queue
        // it will run on (otherwise Current() == nullptr → crash). Same for
        // ReportQp / ReportDropped*: they're guarded by an internal
        // SequenceChecker and assume the calling thread is the task queue.
        // We construct quality_scaler_ inside a posted task and route all
        // ReportQp / ReportDropped* calls via PostTask through this same
        // queue. Destruction is also posted (see destructor).
        {
            cpu_overuse_metrics_observer_ = std::make_unique<NullCpuOveruseMetricsObserver>();
            adaptation_task_queue_ = env_.task_queue_factory().CreateTaskQueue(
                "omni-adaptation",
                webrtc::TaskQueueFactory::Priority::NORMAL);

            // #50 ЭТАП 1, ШАГ 2: СОЗДАЁМ АПСТРИМОВЫЙ RTP-МОДУЛЬ. ПРОВОД НА НЕГО ЕЩЁ НЕ ПЕРЕКЛЮЧЁН.
            //
            // Шаг намеренно только добавляющий: модуль строится и живёт, отправка идёт по-старому.
            // Так проверяется самое неочевидное — что конфигурация и потоковая модель складываются,
            // — до того как тронут путь отправки. Переключение будет следующим шагом.
            //
            // Создание ОБЯЗАНО идти внутри задачи на очереди: модуль берёт очередь в конструкторе
            // (worker_queue_(TaskQueueBase::Current())), и снаружи очереди там оказался бы nullptr.
            // Тот же приём уже используется для NackRequester и QualityScaler ниже.
            // ФАБРИКА КОДИРОВЩИКОВ — ДО ВСЕГО ОСТАЛЬНОГО (#61, шаг D2b).
            //
            // Сам кодировщик мы больше НЕ СОЗДАЁМ: его создаёт VideoStreamEncoder, попросив у этой
            // фабрики, и он же им владеет. От нас нужна только фабрика — и нужна РАНЬШЕ, чем будет
            // послана задача построения оркестратора: под реальными часами она выполняется сразу, и
            // всё, чего к тому моменту нет, для неё просто не существует. Первый замер это и показал:
            // стенды на виртуальных часах работали, а abr-follow не выдавал ни байта, потому что
            // фабрика создавалась ПОЗЖЕ задачи.
            const bool allow_hardware_encode = !force_software_encode_;
            if (!allow_hardware_encode) {
                hardware_encoder_fallback_reason_ = "encode_software=1 requested by configuration";
            }
            encoder_io_stats_ = std::make_shared<OmniEncoderIoStats>();
            encoder_factory_ = std::make_unique<OmniVideoEncoderFactory>(
                allow_hardware_encode, preferred_encode_vendor_id_, encoder_io_stats_,
                test_hw_encode_fail_after_);
            if (!FindSupportedFormat(encoder_factory_->GetSupportedFormats(), codec_kind_)
                         .has_value()) {
                error = std::string("libwebrtc ") + std::string(NativeVideoCodecName(codec_kind_)) +
                        " encoder format is unavailable";
                return false;
            }

            rtp_task_queue_ = env_.task_queue_factory().CreateTaskQueue(
                "omni-rtp",
                webrtc::TaskQueueFactory::Priority::HIGH);
            if (rtp_task_queue_) {
                module_transport_ = std::make_unique<OmniModuleTransport>(this);
                rtcp_transport_ = std::make_unique<OmniRtcpTransport>(this);
                target_rate_observer_ = std::make_unique<OmniTargetRateObserver>();
                encoder_stats_observer_ = std::make_unique<OmniEncoderStatsObserver>();
                push_video_source_ = std::make_unique<OmniPushVideoSource>();
                encoder_sink_ = std::make_unique<OmniEncoderSink>(this);
                bitrate_allocator_factory_ =
                    webrtc::CreateBuiltinVideoBitrateAllocatorFactory();
                rtp_task_queue_->PostTask([this] {
                    // ЗДЕСЬ СТРОИЛСЯ НАШ ПУТЬ ОТПРАВКИ. ЕГО БОЛЬШЕ НЕТ (#57, шаг C2b).
                    //
                    // Двести строк: PacketRouter, TaskQueuePacedSender, ModuleRtpRtcpImpl2,
                    // RTPSenderVideo, регистрация расширения, предел размера пакета, включение
                    // RTX, глубина истории повторов, восстановление нумерации, стартовый темп
                    // пейсера. Каждая из этих строк была найдена замером в тот момент, когда её
                    // отсутствие что-нибудь ломало.
                    //
                    // Всё это теперь делает RtpTransportControllerSend::CreateRtpVideoSender по
                    // ОДНОЙ конфигурации (RtpConfig ниже): те же шесть проводов — шесть полей.
                    // Роутер и пейсер у сборщика свои, история отправки своя, нумерация приходит
                    // и уходит через RtpState.

                    // ПРИЁМНЫЙ МОДУЛЬ: ОТЧЁТЫ, NACK И PLI НАСТОЯЩИМ RTCP.
                    //
                    // Живёт на той же очереди, что и отправляющий: оба привязаны к очереди
                    // конструктором, и держать их на одной проще, чем разбирать два порядка
                    // разрушения. receive_statistics_ считает потери и джиттер для блоков отчёта —
                    // сами мы этого больше не считаем.
                    receive_statistics_ = webrtc::ReceiveStatistics::Create(&env_.clock());
                    if (receive_statistics_) {
                        webrtc::RtpRtcpInterface::Configuration receiver_config;
                        receiver_config.audio = false;
                        receiver_config.receiver_only = true;
                        receiver_config.outgoing_transport = rtcp_transport_.get();
                        receiver_config.local_media_ssrc = kOmniVideoReceiverSsrc;
                        receiver_config.receive_statistics = receive_statistics_.get();
                        receiver_config.rtcp_packet_type_counter_observer = &receiver_rtcp_sent_;
                        receiver_config.non_sender_rtt_measurement = true;
                        receiver_config.rtt_stats = &receiver_rtt_observer_;
                        receiver_rtcp_module_ = webrtc::ModuleRtpRtcpImpl2::CreateReceiveModule(
                            env_,
                            receiver_config,
                            [] { return kOmniVideoSsrc; });
                        if (receiver_rtcp_module_) {
                            // ПРЕДЕЛ ПАКЕТА ЗАДАЁТСЯ И ЗДЕСЬ, НЕ ТОЛЬКО У ОТПРАВЛЯЮЩЕГО.
                            //
                            // RTCPSender режет длинные списки NACK по max_packet_size
                            // (rtcp_sender.cc:76), а у него по умолчанию 1500 — при нашем пределе
                            // датаграммы в 1000. Под потерями список длинный, пакет перерастает
                            // бюджет и гибнет на проводе целиком: просьба о повторе не доходит,
                            // повтора нет, сборка кадров проседает. Замер до этой строки: среднее
                            // 70 собранных кадров из 90 против 87 при обратной связи структурными
                            // событиями.
                            receiver_rtcp_module_->SetMaxRtpPacketSize(max_rtp_packet_bytes_);
                            receiver_rtcp_module_->SetRemoteSSRC(kOmniVideoSsrc);
                            // ОТЧЁТЫ МОЛЧАТ, ПОКА НЕЧЕГО ДОКЛАДЫВАТЬ.
                            //
                            // Сессия, которая только ОТПРАВЛЯЕТ, тоже создаёт этот модуль — они
                            // симметричны. Оставь его говорящим, и он начнёт слать отчёты о
                            // потоке, которого никогда не видел: пустые RR полезут в исходящую
                            // очередь и, поскольку RTCP выдаётся первым, подменят собой видео в
                            // первой же выборке. Замер: набор падал 10 из 10 на «sender report
                            // control failed», потому что вместо видео выгребался чужой отчёт.
                            //
                            // Апстрим держит приёмный модуль только там, где есть приёмный поток.
                            // У нас сессии симметричны, поэтому эквивалент — включать отчёты по
                            // ФАКТУ первого пришедшего медиапакета (см. EnableReceiverRtcpOnce).
                            receiver_rtcp_module_->SetRTCPStatus(webrtc::RtcpMode::kOff);
                        }
                    }
                    // #57 ШАГ C1: СБОРЩИК СТРОИТСЯ, НО ЧЕРЕЗ НЕГО ПОКА НИЧЕГО НЕ ИДЁТ.
                    //
                    // RtpTransportControllerSend — тот самый класс, который соединяет GoogCC,
                    // пейсер, роутер и RTP-модули в одно. За 2026-08-23/24 мы нашли ШЕСТЬ
                    // проводов, которые он ставит сам и которые мы ставили руками, узнавая о
                    // каждом в момент, когда его отсутствие ломало замер. Один из них виден прямо
                    // в его конструкторе: RegisterNotifyBweCallback (rtp_transport_controller_send.cc:134).
                    //
                    // Шаг НАМЕРЕННО только добавляющий, как и первый шаг #50. Он отвечает на самое
                    // неочевидное — складываются ли конфигурация, потоковая модель и порядок
                    // разрушения — ДО того, как тронут путь отправки. Свой пейсер и свой роутер
                    // пока живут и работают; у сборщика они свои, и они неподвижны: он не заводит
                    // ни таймеров, ни регуляторов, пока ему не скажут OnNetworkAvailability или не
                    // зарегистрируют наблюдателя оценки.
                    //
                    // Очередь он берёт в конструкторе (task_queue_(TaskQueueBase::Current())),
                    // поэтому строится ЗДЕСЬ, внутри задачи, — та же причина, что у модуля и
                    // пейсера выше.
                    {
                        webrtc::RtpTransportConfig transport_config{
                            env_, webrtc::BitrateConstraints()};
                        // start_bitrate_bps > 0 требует сам конструктор (RTC_DCHECK на :127).
                        transport_config.bitrate_config.start_bitrate_bps = bitrate_bps_;
                        transport_config.bitrate_config.min_bitrate_bps = kAbrMinBitrateBps;
                        transport_config.bitrate_config.max_bitrate_bps =
                            std::max(bitrate_bps_, negotiated_bitrate_bps_);
                        transport_controller_ =
                            std::make_unique<webrtc::RtpTransportControllerSend>(transport_config);
                    }
                    // ПОРЯДОК: ОРКЕСТРАТОР КОДИРОВАНИЯ СТРОИТСЯ ДО ПРОБУЖДЕНИЯ СБОРЩИКА.
                    //
                    // Сборщик, проснувшись, СРАЗУ отдаёт первую цель — свою стартовую скорость. И
                    // если к этому моменту получателя цели ещё нет, она пропадает: VideoStreamEncoder
                    // остаётся с нулём и НЕ КОДИРУЕТ ВООБЩЕ, только выбрасывает кадры. Замер это и
                    // показал в лоб: encIn 900, encOut 0, encDrop 899, encTgt 0.

                    // #61 ШАГ D2b: ОРКЕСТРАТОР КОДИРОВАНИЯ.
                    //
                    // Строится ЗДЕСЬ, на той же очереди, что и сборщик отправки: его публичные
                    // методы апстрим зовёт с рабочего потока, и у нас эту роль играет rtp-очередь.
                    // Кадры при этом приходят с потока оболочки — источник кладёт их в
                    // VideoStreamEncoder::OnFrame, а тот сам перекладывает работу на свою очередь
                    // кодирования.
                    //
                    // Детектор перегрузки ПЕРЕЕЗЖАЕТ внутрь: адаптация по процессору — его дело, и
                    // держать второй детектор рядом значило бы завести два мнения об одном.
                    // Детектор перегрузки строится ЗДЕСЬ ЖЕ, а не заранее.
                    //
                    // Раньше он создавался ниже по ходу инициализации, а эта задача уже стояла в
                    // очереди — и под РЕАЛЬНЫМИ часами успевала выполниться раньше, чем детектор
                    // появлялся. Тогда весь блок молча пропускался: оркестратор не строился, цель
                    // до кодировщика не доходила, и стенд abr-follow не выдавал ни байта, тогда как
                    // стенды на виртуальных часах работали. Условие «строим только если всё уже
                    // готово» и было ошибкой: готовить надо здесь.
                    if (encoder_factory_ && cpu_overuse_metrics_observer_) {
                        auto overuse_detector = std::make_unique<webrtc::OveruseFrameDetector>(
                            env_, cpu_overuse_metrics_observer_.get());
                        auto encode_queue = env_.task_queue_factory().CreateTaskQueue(
                            "omni-encode", webrtc::TaskQueueFactory::Priority::HIGH);
                        webrtc::TaskQueueBase* encode_queue_view = encode_queue.get();
                        auto cadence = webrtc::FrameCadenceAdapterInterface::Create(
                            &env_.clock(),
                            encode_queue_view,
                            /*metronome=*/nullptr,
                            /*worker_queue=*/rtp_task_queue_.get(),
                            env_.field_trials());

                        webrtc::VideoStreamEncoderSettings encoder_settings(
                            webrtc::VideoEncoder::Capabilities(/*loss_notification=*/false));
                        encoder_settings.encoder_factory = encoder_factory_.get();
                        encoder_settings.bitrate_allocator_factory =
                            bitrate_allocator_factory_.get();

                        video_stream_encoder_ = std::make_unique<webrtc::VideoStreamEncoder>(
                            env_,
                            /*number_of_cores=*/1,
                            encoder_stats_observer_.get(),
                            encoder_settings,
                            std::move(overuse_detector),
                            std::move(cadence),
                            std::move(encode_queue),
                            webrtc::VideoStreamEncoder::BitrateAllocationCallbackType::
                                kVideoBitrateAllocation);

                        video_stream_encoder_->SetSink(encoder_sink_.get(),
                                                       /*rotation_applied=*/true);
                        // ПРОСЬБА СОБЕСЕДНИКА О КЛЮЧЕВОМ КАДРЕ — ОРКЕСТРАТОРУ, ЧЕРЕЗ АПСТРИМОВСКИЙ
                        // ПЕРЕХОДНИК. PLI приходит в отправитель видео байтами RTCP, а тот отдаёт
                        // его тому, кто назначен intra_frame_callback. У апстрима это
                        // EncoderRtcpFeedback (VideoSendStreamImpl), он же держит минимальный
                        // интервал между ключевыми кадрами. Поэтому оркестратор строится РАНЬШЕ
                        // отправителя — тот же порядок, что у VideoSendStreamImpl.
                        encoder_rtcp_feedback_ = std::make_unique<webrtc::EncoderRtcpFeedback>(
                            env_,
                            /*per_layer_keyframes=*/false,
                            std::vector<uint32_t>{kOmniVideoSsrc},
                            video_stream_encoder_.get(),
                            // Нужна только уведомлениям о потерях (LNTF), а мы их не согласуем.
                            [](uint32_t, const std::vector<uint16_t>&) {
                                return std::vector<webrtc::RtpSequenceNumberMap::Info>{};
                            });
                        video_stream_encoder_->SetStartBitrate(bitrate_bps_);
                        video_stream_encoder_->ConfigureEncoder(
                            BuildEncoderConfigUnsafe(), max_rtp_packet_bytes_);
                        // BALANCED для камеры — та же политика, что у апстрима для видеозвонка:
                        // под нехваткой полосы шагают И размер кадра, И частота.
                        //
                        // MAINTAIN_RESOLUTION для экрана — тоже апстримовская, и по той же
                        // причине: уменьшенный текст нечитаем, поэтому за нехватку полосы платят
                        // частотой кадров, а точки держат.
                        video_stream_encoder_->SetSource(
                            push_video_source_.get(),
                            screen_content_
                                ? webrtc::DegradationPreference::MAINTAIN_RESOLUTION
                                : webrtc::DegradationPreference::BALANCED);
                        // ИСТОЧНИК ПОДПИСАН — ТОЛЬКО С ЭТОГО МОМЕНТА КАДР ЕСТЬ КОМУ ВЗЯТЬ.
                        // См. EncodeVideoFrameBuffer: до этого кадры идут через очередь, иначе
                        // первый из них (а он ключевой) теряется в окне между созданием сессии и
                        // запуском оркестратора.
                        encoder_source_ready_.store(true, std::memory_order_release);

                        // ЦЕПОЧКА «ОЦЕНКА КАНАЛА -> КОДИРОВЩИК», СОБРАННАЯ КАК У АПСТРИМА.
                        //
                        // Порядок и величины взяты из VideoSendStreamImpl::OnBitrateUpdated:
                        // сперва цель получает отправитель видео (он делит её между полезной
                        // нагрузкой и защитой), и только ПОТОМ кодировщик получает ту часть,
                        // которая действительно ему причитается. Отдать кодировщику полную цель
                        // значило бы попросить у него больше, чем влезет в канал вместе с
                        // повторами.
                        if (target_rate_observer_) {
                            target_rate_observer_->SetTargetSink(
                                [this](webrtc::TargetTransferRate rate) {
                                    ApplyTargetRateToEncoder(rate);
                                });
                        }
                    }
                    // #57 ШАГ C2a: ОТПРАВИТЕЛЬ ВИДЕО СОЗДАЁТСЯ, КАДРЫ ЧЕРЕЗ НЕГО ЕЩЁ НЕ ИДУТ.
                    //
                    // Опять только добавляющий шаг, и опять по той же причине: он отвечает на
                    // вопросы времени жизни и потоковой модели ДО того, как тронут путь отправки.
                    // Пока не позван SetSending(true), отправитель не регистрируется в роутере
                    // сборщика и ничего не выпускает (rtp_video_sender.cc, SetSending).
                    //
                    // ВСЁ, ЧТО МЫ СТАВИЛИ РУКАМИ, ЗДЕСЬ — ПОЛЯ КОНФИГУРАЦИИ. Отдельный SSRC и тип
                    // нагрузки для RTX, предел пакета, расширение с описателем структуры кадров,
                    // глубина истории повторов: шесть проводов, каждый из которых мы находили
                    // замером, — это шесть строк ниже.
                    if (transport_controller_) {
                        webrtc::RtpConfig rtp_config;
                        rtp_config.ssrcs = {kOmniVideoSsrc};
                        rtp_config.payload_name = std::string(NativeVideoCodecPayloadName(codec_kind_));
                        rtp_config.payload_type =
                            static_cast<int>(NativeVideoCodecPayloadType(codec_kind_));
                        rtp_config.max_packet_size = max_rtp_packet_bytes_;
                        rtp_config.rtx.ssrcs = {kOmniVideoRtxSsrc};
                        rtp_config.rtx.payload_type = kOmniVideoRtxPayloadType;
                        // Ненулевая глубина — условие включения истории отправки, без которой RTX
                        // нечем кормить. Раньше то же самое ставилось вручную через
                        // SetStorePacketsStatus.
                        rtp_config.nack.rtp_history_ms = kSendSideHistoryMs;
                        rtp_config.extensions.emplace_back(
                            std::string(webrtc::RtpGenericFrameDescriptorExtension00::Uri()),
                            kGenericFrameDescriptorExtensionId);

                        // Наблюдатели все нулевые: они просто копируются в конфигурацию модуля
                        // (rtp_video_sender.cc:242-259), где необязательны. Статистику мы снимаем
                        // своей диагностикой.
                        webrtc::RtpSenderObservers observers{};
                        // Кроме двух: PLI собеседника должен дойти до оркестратора, а счётчики
                        // принятого RTCP — до диагностики.
                        observers.intra_frame_callback = encoder_rtcp_feedback_.get();
                        observers.rtcp_type_observer = &sender_rtcp_received_;
                        observers.report_block_data_observer = &sender_report_blocks_;
                        webrtc::RtpSenderFrameEncryptionConfig encryption{};

                        // НУМЕРАЦИЯ И МЕТКА ВРЕМЕНИ ПЕРЕЖИВАЮТ ПЕРЕСОЗДАНИЕ СЕССИИ.
                        //
                        // Тот же инвариант, что раньше держался через SetRtpState на нашем модуле:
                        // SSRC не меняется, значит для собеседника это ОДИН поток, и начать его
                        // нумерацию заново значит выдать ему лавину «потерь». Здесь состояние
                        // отдаётся сборщику при создании, а забирается при разрушении
                        // (GetRtpStates ниже).
                        std::map<uint32_t, webrtc::RtpState> suspended;
                        {
                            std::lock_guard<std::mutex> lock(g_omni_video_rtp_state_mutex);
                            if (g_omni_video_rtp_state_valid) {
                                suspended.emplace(kOmniVideoSsrc, g_omni_video_rtp_state);
                            }
                        }
                        rtp_video_sender_ = transport_controller_->CreateRtpVideoSender(
                            suspended,
                            /*states=*/{},
                            rtp_config,
                            /*rtcp_report_interval_ms=*/1000,
                            module_transport_.get(),
                            observers,
                            std::make_unique<webrtc::FecControllerDefault>(env_),
                            encryption,
                            /*frame_transformer=*/nullptr);
                        if (rtp_video_sender_ && video_stream_encoder_) {
                            video_stream_encoder_->SetFecControllerOverride(rtp_video_sender_);
                        }
                        if (rtp_video_sender_) {
                            // НАЧАЛЬНАЯ МЕТКА ВРЕМЕНИ ВИДНА СНАРУЖИ, И ЭТО НЕ УДОБСТВО СТЕНДА.
                            //
                            // Модуль выбирает её СЛУЧАЙНО — так требует RFC 3550, и это же в нашу
                            // пользу по модели угроз: метка на проводе перестаёт быть временем
                            // съёмки. Но всё, что сопоставляет принятый кадр с исходным (стенд —
                            // по PSNR, оболочка — по липсинку), обязано знать смещение. Раньше
                            // вопрос не стоял: наш путь клал на провод метку съёмки как есть.
                            const auto states = rtp_video_sender_->GetRtpStates();
                            const auto it = states.find(kOmniVideoSsrc);
                            if (it != states.end()) {
                                rtp_start_timestamp_.store(it->second.start_timestamp,
                                                           std::memory_order_relaxed);
                            }
                        }
                    }
                    // #57 ШАГ C2b: ПЕРЕВОРОТ. СБОРЩИК ОЖИВАЕТ ВМЕСТЕ С ПЕРЕНОСОМ КАДРОВ.
                    //
                    // Порядок здесь не стилистический, он вынужденный и проверен замером
                    // 2026-08-24: попытка оживить регулятор ОТДЕЛЬНО (шаг C2b-1) не могла ответить
                    // на свой вопрос — пакеты шли мимо сборщика, история отправки пуста, отчёты не
                    // сопоставлялись ни с чем — и при этом ВРЕДИЛА: ожил второй пейсер на той же
                    // очереди, и доставка упала с 79-90 кадров до 22-25 с двумя падениями из
                    // десяти. Урок тот же, что и на шаге A2: промежуточное состояние с ДВУМЯ
                    // работающими путями хуже обоих концов.
                    //
                    // Поэтому одним куском: наблюдатель, доступность сети, пуск пейсера и
                    // включение отправителя. С этого момента кадры идут ЧЕРЕЗ сборщика
                    // (см. PacketizeEncodedFrame), RTCP приходит ЕМУ (DeliverRtcp), факт отправки
                    // сообщается ЕМУ (OnSentPacket), а цель для ABR берётся у ЕГО наблюдателя.
                    if (transport_controller_) {
                        // СКАЗАТЬ, ЧТО ОБРАТНАЯ СВЯЗЬ — RFC 8888. БЕЗ ЭТОГО ОНА НЕ РАБОТАЕТ.
                        //
                        // Строка выглядит согласовательной формальностью, а на деле она включает
                        // ТРАНСПОРТНУЮ НУМЕРАЦИЮ пакетов: SetPreferredRtcpCcAckType зовёт
                        // packet_router_.ConfigureForRtcpFeedback(set_transport_seq=true)
                        // (rtp_transport_controller_send.cc:644). Без номера у пакета нет
                        // packet_id, факт отправки сообщить нечем, история отправки остаётся без
                        // времён — и отчёт собеседника не сопоставляется НИ С ЧЕМ.
                        //
                        // Как это выглядит в замере: отчёты приходят все (238 из 238), оценка
                        // стоит на стартовой 1.5 Мбит/с при канале 600 кбит/с, и прогон с
                        // применением отчёта не отличается от прогона без него ничем. Ровно та же
                        // молчаливая картина, что и «ccfbApplied 79/0» на перевороте #50 — и
                        // ровно поэтому в наборе стоит проверка «оценка обязана лечь НИЖЕ
                        // запроса», а не «отчёт дошёл».
                        transport_controller_->SetPreferredRtcpCcAckType(
                            webrtc::RtcpFeedbackType::CCFB);
                        transport_controller_->RegisterTargetTransferRateObserver(
                            target_rate_observer_.get());
                        // ПРОБЫ В ПЕРИОДЫ, КОГДА СЛАТЬ НЕЧЕГО.
                        //
                        // Про демонстрацию экрана в первую очередь: статичная картинка означает,
                        // что отправитель ограничен НЕ каналом, а содержимым, и без проб оценка
                        // в такие периоды не растёт вовсе — сколько бы свободной полосы ни было.
                        // Раньше то же самое просилось у нашего регулятора через StreamsConfig.
                        transport_controller_->EnablePeriodicAlrProbing(true);
                        transport_controller_->OnNetworkAvailability(true);
                        // Пейсер до этого стоит: EnsureStarted переводит его в рабочий режим.
                        transport_controller_->EnsureStarted();
                    }
                    if (rtp_video_sender_) {
                        // Регистрирует модуль в роутере сборщика и включает наблюдателя обратной
                        // связи. До этого OnEncodedImage возвращает ERROR_SEND_FAILED
                        // (rtp_video_sender.cc: `if (!active_)`).
                        rtp_video_sender_->SetSending(true);
                    }

                    transport_controller_created_.store(transport_controller_ != nullptr &&
                                                            rtp_video_sender_ != nullptr,
                                                        std::memory_order_relaxed);
                    // «Модуль отправки поднялся» теперь означает «поднялся отправитель видео у
                    // сборщика»: своего модуля у нас нет.
                    module_created_.store(rtp_video_sender_ != nullptr,
                                          std::memory_order_relaxed);
                    receiver_rtcp_ready_.store(receiver_rtcp_module_ != nullptr,
                                               std::memory_order_relaxed);
                });
            }
            // ДЕТЕКТОР ПЕРЕГРУЗКИ И МАСШТАБИРОВЩИК КАЧЕСТВА ПЕРЕЕХАЛИ В ОРКЕСТРАТОР (#61, D2b).
            //
            // Оба заводились здесь и слали сигналы нашей лестнице ABR. Теперь и то и другое —
            // внутренности VideoStreamEncoder: он держит их в VideoStreamEncoderResourceManager
            // вместе с ограничителями по полосе и по числу точек, и сводит их решения в ОДНО.
            // Держать их снаружи значило бы завести два мнения об одном и том же и складывать их
            // руками — ровно та самая лестница, которую мы уже один раз сняли.
        }

        // Wave 5b (2026-05-18): NackRequester. Lives on adaptation_task_queue_
        // (sequence checker binds in the constructor). NackPeriodicProcessor
        // is a shared 20ms ticker that owns its RepeatingTaskHandle on the
        // queue where the first RegisterNackModule was called — so we
        // construct everything inside a single PostTask on the adaptation
        // queue, guaranteeing the processor and the requester pin to the
        // same sequence. ScopedNackPeriodicProcessorRegistration inside
        // NackRequester does the register/unregister automatically.
        if (adaptation_task_queue_) {
            receiver_rtcp_feedback_ = std::make_unique<OmniReceiverRtcpFeedback>(
                [this](const std::vector<uint16_t>& sequence_numbers) {
                    SendReceiverNack(sequence_numbers);
                },
                [this] { OnNackRequesterAsksForKeyframe(); });
            adaptation_task_queue_->PostTask([this] {
                nack_periodic_processor_ = std::make_unique<webrtc::NackPeriodicProcessor>();
                nack_requester_ = std::make_unique<webrtc::NackRequester>(
                    adaptation_task_queue_.get(),
                    nack_periodic_processor_.get(),
                    &env_.clock(),
                    receiver_rtcp_feedback_.get(),
                    receiver_rtcp_feedback_.get(),
                    env_.field_trials());
            });
        }

        // БУФЕР ПОКАЗА — АПСТРИМОВСКИЙ ЦЕЛИКОМ (#60).
        //
        // Своя очередь: контроллер держит на ней сторож таймаутов и планировщик показа —
        // повторяющиеся и отложенные задачи, — и все его методы обязаны зваться оттуда. Класть их
        // на adaptation_task_queue_ было бы можно, но там живут детектор перегрузки и
        // масштабировщик качества ОТПРАВИТЕЛЯ; смешивать приём с отправкой на одной очереди значит
        // связать их задержки без всякой на то причины.
        //
        // Размеры буфера и сроки ожидания задаются САМИМ контроллером (kMaxFramesBuffered = 800,
        // kMaxFramesHistory = 1 << 13) — наши прежние константы вместе с ними и ушли.
        playout_task_queue_ = env_.task_queue_factory().CreateTaskQueue(
            "omni-video-playout", webrtc::TaskQueueFactory::Priority::NORMAL);
        playout_timing_ = std::make_unique<webrtc::VCMTiming>(&env_.clock(), env_.field_trials());
        playout_stats_ = std::make_unique<OmniPlayoutStatsObserver>();
        playout_receiver_ = std::make_unique<OmniPlayoutReceiver>(this);
        if (playout_task_queue_) {
            playout_task_queue_->PostTask([this] {
                buffer_controller_ = std::make_unique<webrtc::VideoStreamBufferController>(
                    &env_.clock(),
                    playout_task_queue_.get(),
                    playout_timing_.get(),
                    playout_stats_.get(),
                    playout_receiver_.get(),
                    webrtc::TimeDelta::Millis(kMaxWaitForKeyframeMs),
                    webrtc::TimeDelta::Millis(kMaxWaitForFrameMs),
                    std::make_unique<webrtc::TaskQueueFrameDecodeScheduler>(
                        &env_.clock(), playout_task_queue_.get()),
                    env_.field_trials());
            });
        }

        // Приёмная половина оценки канала. Готовые отчёты складываются в буфер, откуда их забирает
        // оболочка и передаёт отправителю тем же путём, что и NACK, — своего транспорта у медиа нет
        // и не должно быть.
        feedback_generator_ = std::make_unique<webrtc::CongestionControlFeedbackGenerator>(
            env_,
            [this](std::vector<std::unique_ptr<webrtc::rtcp::RtcpPacket>> packets) {
                for (const auto& packet : packets) {
                    if (!packet) {
                        continue;
                    }
                    // 1200 байт — обычный предел для RTCP: отчёт, не влезший в него, апстрим
                    // сам разрежет на несколько вызовов. Каждый вызов — самостоятельный
                    // разбираемый блок, поэтому кладём его отдельным элементом: это и есть та
                    // граница, по которой потом можно резать без порчи.
                    packet->Build(1200u, [this](std::span<const uint8_t> data) {
                        if (data.empty()) {
                            return;
                        }
                        // ОТЧЁТ ЕДЕТ БЫСТРЫМ ПУТЁМ — ТЕМ ЖЕ, ЧТО ОСТАЛЬНОЙ RTCP ПРИЁМНИКА.
                        //
                        // Копился в отдельном буфере, который оболочка выгребала управляющим
                        // циклом раз в 50 мс и везла отдельным контрольным сообщением; на той
                        // стороне его подбирал такой же 50-миллисекундный опрос. Два периода
                        // целиком входили в измеренный медиа-RTT: на loopback при пути 0.1 мс
                        // получалось 87–164 мс. По этому числу ABR принимает решения.
                        //
                        // «Опрашивать чаще» проверено и ОПРОВЕРГНУТО: на 15 мс оценщик начинал
                        // метаться (264 смены цели за 30 с против 3–11). Частоту отчётов задаёт
                        // генератор, её трогать нельзя — трогать надо задержку. С 2026-09-26 отчёт
                        // уходит в ядро сразу, без очереди (см. OmniPacketSink).
                        SendOutgoingRtcp(data.data(), data.size());
                        // СКОЛЬКО ОТЧЁТОВ ГЕНЕРАТОР ВООБЩЕ ПОСТРОИЛ.
                        //
                        // Без этого числа «отчётов не видно» не разделяется на «генератор
                        // молчит» и «построил, но не доехало»: все прочие счётчики стоят ниже
                        // по течению и в обоих случаях показывают ноль. Ровно на этом встала
                        // попытка быстрого пути 2026-08-31 — см.
                        // docs/bugs/datagrams-we-counted-as-sent-never-left.md.
                        transport_feedback_reports_built_.fetch_add(1, std::memory_order_relaxed);
                    });
                }
            });

        // СВОЕГО РЕГУЛЯТОРА ЗДЕСЬ БОЛЬШЕ НЕТ (#57, шаг C2b).
        //
        // Строился второй экземпляр GoogCC, ему отдавались RTT и слепленный из доли потерь пира
        // отчёт, а ответ применялся к нашему пейсеру. Всё это принадлежит
        // RtpTransportControllerSend: он держит свой GoogCC, свой пейсер и свой пробинг, и
        // кормится фактической доставкой каждого пакета вместо пересчёта чужих долей.
        //
        // Запуск пробинга (OnNetworkAvailability + StreamsConfig с requests_alr_probing) переехал
        // туда же: сборщик зовёт то же самое сам в OnNetworkAvailability, а ALR-пробы включаются
        // через EnablePeriodicAlrProbing. Оба вызова стоят рядом с переворотом.

        // H.264 and HEVC: no software decoder in the build — only the Media Foundation one, built
        // on the first frame or when the GPU render path attaches. The receiver exists without
        // `decoder_`.
        if (!HasSoftwareVideoCodec(codec_kind_)) {
            return true;
        }
        decoder_factory_ = webrtc::CreateBuiltinVideoDecoderFactory();
        if (!decoder_factory_) {
            error = "libwebrtc decoder factory creation failed";
            return false;
        }

        std::optional<webrtc::SdpVideoFormat> decoder_format =
            FindSupportedFormat(decoder_factory_->GetSupportedFormats(), codec_kind_);
        if (!decoder_format.has_value()) {
            error = std::string("libwebrtc ") + std::string(NativeVideoCodecName(codec_kind_)) +
                " decoder format is unavailable";
            return false;
        }

        decoder_ = decoder_factory_->Create(env_, *decoder_format);
        if (!decoder_) {
            error = std::string("libwebrtc ") + std::string(NativeVideoCodecName(codec_kind_)) +
                " decoder creation failed";
            return false;
        }

        // СВОЙ EncoderBitrateAdjuster УБРАН (#61, шаг D3).
        //
        // Он ставился, когда кодировщик был наш: мы просили у него скорость, он переливал, а
        // корректор вёл трёхсекундное окно фактического выхода и правил следующую просьбу.
        // Кодировщиком владеет VideoStreamEncoder, и ровно такой же корректор стоит ВНУТРИ него
        // (video_stream_encoder.cc: bitrate_adjuster_), причём на том же самом объекте апстрима.
        // Наш экземпляр после переворота кормить стало нечем — единственная точка, где он получал
        // размер кадра, ушла вместе с PacketizeEncodedFrame, — и он молча считал пустоту.
        if (decoder_->RegisterDecodeCompleteCallback(&decode_callback_) != WEBRTC_VIDEO_CODEC_OK) {
            error = std::string("libwebrtc ") + std::string(NativeVideoCodecName(codec_kind_)) +
                " decoder callback registration failed";
            return false;
        }

        webrtc::VideoDecoder::Settings decoder_settings;
        decoder_settings.set_codec_type(NativeVideoCodecType(codec_kind_));
        decoder_settings.set_max_render_resolution({width_, height_});
        decoder_settings.set_number_of_cores(1);
        if (!decoder_->Configure(decoder_settings)) {
            error = std::string("libwebrtc ") + std::string(NativeVideoCodecName(codec_kind_)) +
                " decoder Configure failed";
            return false;
        }

        return true;
    }

    /// ЦЕЛЬ ПО РАЗМЕРУ КАДРА И ЧАСТОТЕ — ЭТО ПОЖЕЛАНИЯ ОРКЕСТРАТОРА КОДИРОВАНИЯ (#61, шаг D3).
    ///
    /// Здесь стояли три наши функции: ступень разрешения по битрейту, пауза перед подъёмом и
    /// минимально осмысленный битрейт для размера. Всё это — политика, и вся она у апстрима есть:
    /// решение принимает VideoStreamEncoderResourceManager, паузу между шагами держит
    /// VideoStreamAdapter, а «хватает ли кадру бит» судит QualityScaler по QP кадра.
    ///
    /// Выражается решение пожеланиями ИСТОЧНИКУ: не больше стольких точек, не чаще стольких
    /// кадров. У апстрима адресат — VideoAdapter внутри источника захвата; у нас источник захвата
    /// живёт в оболочке, поэтому пожелание едет к ней через FFI как цель ABR. Смысл тот же, и
    /// петля замыкается так же: оболочка уменьшает кадр по дороге к кодировщику, оркестратор
    /// видит уменьшённый кадр и следующий шаг считает уже от него.
    void ApplyEncoderSinkWantsUnsafe()
    {
        const int ceiling_pixels = negotiated_width_ * negotiated_height_;
        if (ceiling_pixels <= 0) {
            return;
        }

        // Потолок отправителя — абсолютная граница, ниже которой пожелания могут опускаться,
        // а выше не поднимаются никогда. Пожелания приходят «не больше» с максимумом int,
        // когда ограничения нет, поэтому просто берём меньшее.
        int max_pixels = ceiling_pixels;
        int max_fps = negotiated_fps_;
        if (push_video_source_) {
            const webrtc::VideoSinkWants wants = push_video_source_->wants();
            if (wants.max_pixel_count > 0) {
                max_pixels = std::min(max_pixels, wants.max_pixel_count);
            }
            if (wants.max_framerate_fps > 0) {
                max_fps = std::min(max_fps, wants.max_framerate_fps);
            }
        }

        int width = negotiated_width_;
        int height = negotiated_height_;
        if (max_pixels < ceiling_pixels) {
            // Масштабируем ВЫБОР ОТПРАВИТЕЛЯ до нужной площади, а не подставляем размер из
            // таблицы: соотношение сторон принадлежит источнику (16:9 у камеры, что угодно у окна
            // на экране), и подмена его табличным растянула бы картинку.
            const double scale = std::sqrt(
                static_cast<double>(max_pixels) / static_cast<double>(ceiling_pixels));
            const int scaled_width = static_cast<int>(negotiated_width_ * scale);
            const int scaled_height = static_cast<int>(negotiated_height_ * scale);
            // Чётные размеры: I420/NV12 несут цветность вдвое реже, нечётная сторона незаконна.
            width = std::max(2, scaled_width - (scaled_width % 2));
            height = std::max(2, scaled_height - (scaled_height % 2));
        }

        if (width != abr_target_width_ || height != abr_target_height_) {
            abr_target_width_ = width;
            abr_target_height_ = height;
            ++abr_resolution_changes_;
        }
        // Пола частоты у нас больше нет своего: сколько кадров оркестратор готов принять, столько
        // и просим. Прежний пол в 24 кадра отменял бы половину политики BALANCED, которая под
        // нехваткой полосы шагает и размером, и частотой; у самого апстрима пол равен двум.
        abr_target_fps_ = std::max(1, std::min(max_fps, negotiated_fps_));
    }

    /// Кто именно попросил уменьшить — говорит сам оркестратор, а не наша догадка по замеру.
    ///
    /// Он считает шаги адаптации ОТДЕЛЬНО по процессору и по качеству
    /// (VideoStreamEncoderObserver::OnAdaptationChanged), и это разные починки: перегрузка
    /// процессора не лечится битрейтом, а нехватка полосы не лечится более быстрой машиной.
    /// Смотрим на прирост шагов с прошлого такта — счётчики монотонные.
    void NoteEncoderAdaptationUnsafe()
    {
        if (!encoder_stats_observer_) {
            return;
        }
        const int cpu_steps = encoder_stats_observer_->cpu_resolution_steps() +
                              encoder_stats_observer_->cpu_framerate_steps();
        const int quality_steps = encoder_stats_observer_->quality_resolution_steps() +
                                  encoder_stats_observer_->quality_framerate_steps();
        if (cpu_steps > abr_seen_cpu_steps_) {
            abr_state_ = OMNI_WEBRTC_MEDIA_ABR_STATE_DOWN_LOCAL_PROCESSING;
            SetAbrReasonUnsafe("encoder:cpu");
        } else if (quality_steps > abr_seen_quality_steps_) {
            abr_state_ = OMNI_WEBRTC_MEDIA_ABR_STATE_DOWN_MODERATE;
            SetAbrReasonUnsafe("encoder:quality");
        }
        abr_seen_cpu_steps_ = cpu_steps;
        abr_seen_quality_steps_ = quality_steps;
    }

    void FillAbrTargetUnsafe(OmniWebRtcMediaAbrTarget& target, bool changed) const
    {
        target.changed = changed ? 1u : 0u;
        target.target_bitrate_bps = static_cast<uint32_t>(std::max(1, abr_target_bitrate_bps_));
        target.target_fps = static_cast<uint32_t>(std::max(1, abr_target_fps_));
        target.target_width = static_cast<uint32_t>(std::max(1, abr_target_width_));
        target.target_height = static_cast<uint32_t>(std::max(1, abr_target_height_));
        target.state = abr_state_;
    }

    // ЛЕСТНИЦА РАЗРЕШЕНИЙ (1280x720 ... 320x180) И ТРИ МЕХАНИЗМА, ДВИГАВШИЕ ЕЁ ОДНОВРЕМЕННО,
    // СНЯТЫ ОКОНЧАТЕЛЬНО (#61, шаг D3).
    //
    // Живой замер 2026-08-17 (город А <-> город Б, 121 решение ABR) показал, что она не работала ни
    // разу: КАЖДЫЙ шаг вверх немедленно отменялся шагом вниз, пятнадцать циклов подряд, со сносом
    // 1280x720 -> 320x180 при loss=0, drops=0 и пустой очереди. Причина — условие
    // local_processing: порог 33-41 мс против замеренных cap 47-91 мс и enc 55-97 мс на ЛЮБОЙ
    // ступени, включая 320x180. То есть оно истинно всегда, а второй половиной служил низкий
    // sent_fps, который у демонстрации экрана означает «на экране ничего не изменилось», а вовсе
    // не «мы не успеваем».
    //
    // После того замера размер кадра до конца сеанса задавал отправитель, а адаптации по размеру
    // не было вовсе. Теперь её ведёт VideoStreamEncoderResourceManager — см.
    // ApplyEncoderSinkWantsUnsafe. Выбор отправителя остался ПОТОЛКОМ: выше него не поднимаемся.
    /// ПОТОЛОК ПО ИЗМЕРЕННОЙ ЁМКОСТИ СНЯТ (2026-08-22).
    ///
    /// Здесь стояли AbrBitrateCeilingBpsUnsafe / NoteAbrCapacityBrokeAtUnsafe /
    /// NoteAbrCapacityHeldAtUnsafe: своя оценка ёмкости канала, которая зажимала цель GoogCC.
    /// Требование, из которого они родились, ВЕРНОЕ и остаётся в силе — согласованный профиль это
    /// пожелание, а не свойство канала, и восстановление не имеет права разгоняться обратно в тот
    /// же затор (docs/bugs/call-drowns-the-mobile-uplink.md: 1.15 МБ за 4 с, RTT 134 -> 808 мс,
    /// двадцать секунд тишины).
    ///
    /// Сменился ИСПОЛНИТЕЛЬ этого требования. Оценивать ёмкость канала — работа
    /// LinkCapacityEstimator внутри GoogCC, а держаться границ — работа самого регулятора:
    /// min/max/starting заданы ему в NetworkControllerConfig.constraints при создании.
    /// Самописный вариант был храповиком: вниз по любой пробе с затором, вверх не больше чем на 25%
    /// за стабильное окно >= 4 с. От пола 150 кбит/с до 8 Мбит/с это 18 шагов, больше 70 секунд
    /// ползком, и ровно он обезвреживал пробы ProbeController — механизм, который существует, чтобы
    /// выходить из ямы прыжком.

    // Wave 4 Phase 2b: convert an ABR sample into GoogCC inputs and return
    // the latest target rate. Returns 0 if the controller wasn't constructed
    // (legacy build without GoogCC obj files) — caller falls back to legacy
    // logic.
    //
    // GoogCC expects periodic feedback. Our UpdateAbr fires once per
    // Receiver Report (~1-2 s in steady state). We feed three messages
    /// Спросить у сборщика, сколько канал держит сейчас.
    ///
    /// #57 ШАГ C2b: ЗДЕСЬ БЫЛ ВТОРОЙ РЕГУЛЯТОР, И ЕГО БОЛЬШЕ НЕТ.
    ///
    /// Функция кормила НАШ экземпляр GoogCC: отдавала ему RTT, слепленный из доли потерь пира
    /// отчёт и такт обработки, а ответ применяла к нашему пейсеру. Всё это теперь делает
    /// RtpTransportControllerSend, и делает точнее — по фактической доставке каждого пакета
    /// (RFC 8888), а не по доле потерь, пересчитанной из чужого такта.
    ///
    /// Своей очереди отправки больше нет (пакет уходит в ядро прямо из транспорта, см.
    /// OmniPacketSink), так что и слагаемого «утонуло у нас» здесь нет.
    int RouteSampleThroughGoogCcUnsafe(int64_t now_ms)
    {
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
        int last_target_bps = 0;
        // ЦЕЛЬ СБОРЩИКА БЕРЁТСЯ ТОЛЬКО ПОСЛЕ ДОКАЗАТЕЛЬСТВА ДОСТАВКИ.
        //
        // Сборщик начинает отдавать цель СРАЗУ — и первая его цель это стартовая скорость из
        // конфигурации, то есть наша же просьба, а не измерение. Взять её как мнение значит
        // получить подъём без единого доказательства, что путь несёт. Замер поймал это в лоб:
        // после отката по молчащей обратной связи (4.0 -> 3.4 Мбит/с) следующий же такт вернул
        // цель на 4.0, потому что сборщик «сказал» стартовую. Ровно тот сценарий, который топил
        // мобильный аплинк (docs/bugs/call-drowns-the-mobile-uplink.md).
        //
        // Условие простое и проверяемое: хотя бы один отчёт о доставке применён. До этого у
        // сборщика нет входных данных, и его число — не оценка канала, а эхо конфигурации.
        const bool delivery_proven = transport_feedback_reports_applied_ > 0;
        if (delivery_proven && target_rate_observer_ && target_rate_observer_->updates() > 0) {
            goog_cc_feedback_target_bps_ =
                static_cast<int>(target_rate_observer_->target_bps());
        }
        if (goog_cc_feedback_target_bps_ > 0 &&
            goog_cc_feedback_target_bps_ != goog_cc_feedback_target_consumed_) {
            last_target_bps = goog_cc_feedback_target_bps_;
        }
        goog_cc_feedback_target_consumed_ = goog_cc_feedback_target_bps_;

        // СРОК ГОДНОСТИ ОЦЕНКИ. Регулятор отдаёт цель только при её ИЗМЕНЕНИИ, поэтому «в этот раз
        // ничего не сказал» значит «мнение прежнее», а не «мнения нет» — но лишь пока его
        // подпитывают. Механизм тот же, что был; изменился только источник числа.
        if (last_target_bps > 0) {
            goog_cc_last_target_bps_ = last_target_bps;
            goog_cc_last_target_at_ms_ = now_ms;
        } else if (goog_cc_last_target_bps_ > 0 &&
                   now_ms - goog_cc_last_target_at_ms_ <= kGoogCcTargetMemoryMs) {
            last_target_bps = goog_cc_last_target_bps_;
        }
        // Ноль означает «мнения нет»: либо сборщик не сказал ещё ничего, либо сказанное
        // протухло. И то и другое вызывающий читает одинаково — действуем по своей политике.
        return last_target_bps;
#else
        (void)now_ms;
        return 0;
#endif
    }

    void UpdateAbrTrendUnsafe(double loss_q8, double rtt_ms, double jitter)
    {
        abr_loss_ewma_q8_ = UpdateAbrEwma(abr_loss_ewma_q8_, loss_q8, loss_q8 <= 0.0 ? 0.55 : 0.20);
        abr_rtt_ewma_ms_ = UpdateAbrEwma(abr_rtt_ewma_ms_, rtt_ms, rtt_ms < abr_rtt_ewma_ms_ ? 0.35 : 0.18);
        abr_jitter_ewma_ = UpdateAbrEwma(abr_jitter_ewma_, jitter, jitter <= 0.0 ? 0.55 : 0.18);
    }

    static double UpdateAbrEwma(double previous, double next, double alpha)
    {
        if (previous <= 0.0) {
            return std::max(0.0, next);
        }

        return previous + (std::max(0.0, next) - previous) * alpha;
    }

    // AbrReasonForSampleUnsafe снят вместе с лестницей: он переводил в текст её пороги
    // (down:hard_loss, down:loss_window, down:rtt_trend, down:jitter_window, down:queue_window),
    // а порогов больше нет. Причина решения теперь называется там, где решение принимается:
    // googcc:down / googcc:up / googcc:hold для канала и local:encode_overrun для нашей
    // перегрузки. Диагностика при этом не обеднела — сырые тренды abr_*_ewma_* по-прежнему
    // публикуются в diagnostics_json, просто решений по ним мы больше не принимаем.

    void SetAbrReasonUnsafe(std::string reason)
    {
        if (reason.empty()) {
            reason = "stable";
        }

        if (abr_reason_ == reason) {
            return;
        }

        abr_reason_ = std::move(reason);
        abr_reason_history_.push_back(abr_reason_);
        while (abr_reason_history_.size() > kAbrReasonHistoryLimit) {
            abr_reason_history_.pop_front();
        }
    }

    std::string AbrReasonHistoryJsonUnsafe() const
    {
        std::ostringstream history;
        history << '[';
        bool first = true;
        for (const std::string& reason : abr_reason_history_) {
            if (!first) {
                history << ',';
            }

            first = false;
            history << '"' << JsonEscape(reason) << '"';
        }

        history << ']';
        return history.str();
    }

    /// Заголовок первого КОДИРОВАННОГО СРЕЗА в кадре (тип NAL 1 или 5).
    ///
    /// Нужен ради nal_ref_idc: это единственный признак «на кадр никто не ссылается», который есть
    /// в самом потоке и не требует ни сигнализации в RTP, ни доверия к настройкам кодека.
    ///
    /// Именно СРЕЗА, а не первого попавшегося NAL. Аппаратный кодировщик добавляет впереди SEI и
    /// разделитель доступа, у которых nal_ref_idc равен нулю ВСЕГДА, — и по первому NAL все 90
    /// кадров из 90 выглядели непривязанными, что бессмысленно. Программный путь таких NAL не
    /// добавляет, поэтому по нему та же ошибка была не видна.
    static std::optional<uint8_t> FirstSliceNalHeader(std::span<const uint8_t> data)
    {
        for (size_t i = 0; i + 3 < data.size(); ++i) {
            if (data[i] != 0x00 || data[i + 1] != 0x00) {
                continue;
            }

            size_t header_at = 0;
            if (data[i + 2] == 0x01) {
                header_at = i + 3;
            } else if (data[i + 2] == 0x00 && i + 4 < data.size() && data[i + 3] == 0x01) {
                header_at = i + 4;
            } else {
                continue;
            }

            if (header_at >= data.size()) {
                break;
            }

            const uint8_t nal_type = data[header_at] & 0x1Fu;
            if (nal_type == 1u || nal_type == 5u) {
                return data[header_at];
            }
        }
        return std::nullopt;
    }

public:
    /// Сообщить сборщику, что пакет ушёл на провод. Зовётся из транспорта модуля.
    ///
    /// Зовётся снаружи из OmniModuleTransport — тем же приёмом, что и остальные точки транспорта.
    void NotifyModulePacketSent(
        std::span<const uint8_t> packet, const webrtc::PacketOptions& options)
    {
        const webrtc::Timestamp now = env_.clock().CurrentTime();

        // ЧТО ИМЕННО УШЛО НА ПРОВОД, ПО ВИДАМ.
        //
        // Единственная точка, через которую проходит КАЖДЫЙ отправленный пакет. Раньше счёт шёл в
        // обратном вызове НАШЕГО роутера; роутер теперь принадлежит сборщику, и его единственный
        // обратный вызов занят самим сборщиком (RegisterNotifyBweCallback в его конструкторе) —
        // перехватывать его значило бы отобрать у него оценку канала.
        //
        // Различаем по двум признакам, и оба точные:
        //   options.is_media  — апстрим ставит его ровно для свежего медиа, исключая повторы,
        //                       добивку и FEC (rtp_sender_egress.cc:269);
        //   бит P в заголовке — отличает добивку от повтора: добивка несёт байты набивки, повтор
        //                       несёт исходный пакет.
        if (packet.size() >= kRtpHeaderBytes && !options.is_media) {
            const uint32_t ssrc = ReadBigEndian32(packet.data() + 8);
            const bool has_padding = (packet[0] & 0x20u) != 0u;
            if (has_padding) {
                module_padding_sent_.fetch_add(1, std::memory_order_relaxed);
            } else if (ssrc == kOmniVideoRtxSsrc) {
                module_retransmissions_sent_.fetch_add(1, std::memory_order_relaxed);
            }
        }

        if (!transport_controller_ || options.packet_id < 0) {
            return;
        }
        // ВРЕМЯ ОТПРАВКИ ХРАНИМ И САМИ — РОВНО РАДИ RTT.
        //
        // Всё остальное о доставке считает сборщик, и своей истории отправки у нас больше нет. Но
        // RTT он наружу не отдаёт: у апстрима эту величину разносит Call через CallStats, которого
        // у нас нет. А нужна она в двух местах — запросчику NACK на приёмной стороне (его переносит
        // оболочка) и правилам ABR вида «rtt >= 350». Поэтому здесь остаётся кольцо из времён
        // отправки, и только оно: минимальный проход считается по нему в ApplyTransportFeedback.
        {
            std::lock_guard<std::mutex> lock(feedback_mutex_);
            // КЛЮЧ — НОМЕР RTP, А НЕ ТРАНСПОРТНЫЙ.
            //
            // Отчёт RFC 8888 отчитывается по (SSRC, номер RTP), а не по транспортному номеру: это
            // и есть его отличие от TWCC. Ключ по packet_id не совпал бы ни разу, и RTT так и
            // остался бы неизмеренным — тихо, потому что «не нашли в кольце» неотличимо от
            // «пакета не было».
            if (packet.size() >= kRtpHeaderBytes) {
                const uint32_t media_ssrc = ReadBigEndian32(packet.data() + 8);
                if (media_ssrc == ssrc_) {
                    // РАЗВЁРНУТЫЙ НОМЕР, ИНАЧЕ ПОСЛЕ ПЕРЕПОЛНЕНИЯ ВЫТЕСНЯЕТСЯ СВЕЖЕЕ.
                    //
                    // Ключ был uint16_t, а вытеснение — `erase(begin())`, то есть наименьший номер.
                    // Пока номера растут, это и есть старейший. Но через 65 536 пакетов (минуты
                    // звонка) номер переходит через ноль, и наименьшими становятся как раз новые:
                    // каждая вставка тут же сама себя стирала, и RTT переставал обновляться.
                    sent_at_by_packet_id_.insert_or_assign(
                        sent_sequence_unwrapper_.Unwrap(ReadBigEndian16(packet.data() + 2)), now);
                    if (sent_at_by_packet_id_.size() > kMaxSentAtEntries) {
                        sent_at_by_packet_id_.erase(sent_at_by_packet_id_.begin());
                    }
                }
            }
        }

        webrtc::SentPacketInfo sent;
        sent.send_time_ms = now.ms();
        sent.packet_id = options.packet_id;
        sent.info.included_in_feedback = options.included_in_feedback;
        sent.info.included_in_allocation = options.included_in_allocation;
        sent.info.packet_size_bytes = static_cast<int64_t>(packet.size());
        // Сборщик сам перекладывает вызов на свою очередь, если пришли не с неё
        // (rtp_transport_controller_send.cc: OnSentPacket).
        transport_controller_->OnSentPacket(sent);
    }

    /// Применить ответ регулятора ЦЕЛИКОМ — единственное место, где это делается.
    ///
    /// В апстриме тем же занят RtpTransportControllerSend::PostUpdates. У нас ответ
    /// разбирался в трёх местах и в каждом по-своему: где-то брали только pacer_config,
    /// где-то только target_rate, а probe_cluster_configs не читали нигде — то есть регулятор
    /// просил пробы, а мы выбрасывали просьбу молча. Разбор ответа в одном месте — не
    /// украшение: он делает невозможным целый класс ошибок этой сессии, где поле у ответа
    /// есть, а читателя у него нет.
    /// Отдать проход туда-обратно запросчику NACK и буферу показа — то, что у апстрима делает
    /// VideoReceiveStream2::OnRttUpdate. Источник — приёмный модуль (OmniRttObserver).
    ///
    /// Таймер повтора строится от RTT: пока запросчик его не знает, повторный запрос на пакет,
    /// чья ретрансмиссия тоже погибла, фактически не уходит — замер на 20% потерь дал 84
    /// запроса на 83 потери, то есть ровно по одному на потерю.
    void ApplyReceiverRtt(int64_t rtt_ms)
    {
        // НОЛЬ — НЕ ИЗМЕРЕНИЕ, А ЗНАЧЕНИЕ НИЖЕ РАЗРЕШЕНИЯ ИЗМЕРЕНИЯ. ОТДАВАТЬ ЕГО НЕЛЬЗЯ.
        //
        // NackRequester строит от RTT таймер повтора: условие «пора переспросить снова» это
        // `now - sent_at_time >= rtt_` (nack_requester.cc:271). При rtt_ == 0 оно истинно НА
        // КАЖДОМ такте, и один и тот же пакет переспрашивается непрерывно до kMaxNackRetries=100.
        //
        // ОГОВОРКА, ЧТОБЫ НЕ ВВОДИТЬ В ЗАБЛУЖДЕНИЕ: ОБВАЛ ДОСТАВКИ — НЕ ОТСЮДА.
        //
        // Здесь стояло объяснение, что переспросы втрое чаще потерь съедают полосу и потому
        // рушится сборка. Правдоподобно и НЕВЕРНО: правка не изменила ни одного числа в замере,
        // побитово. Настоящей причиной оказался буфер показа — см. kReasonDecodeTimeout и
        // RequestReceiverKeyframeUnsafe(force).
        //
        // Правка тем не менее верна по существу и остаётся: ноль — не измерение, а значение ниже
        // разрешения, и отдавать его вместо умолчания нельзя.
        //
        // У апстрима на случай «RTT ещё не знаем» есть свой умолчательный kDefaultRtt = 100 мс
        // (nack_requester.cc:36). Не перетирать его нулём — значит оставить работать именно то
        // поведение, которое апстрим и задумал.
        if (rtt_ms <= 0 || adaptation_task_queue_ == nullptr) {
            return;
        }
        adaptation_task_queue_->PostTask([this, rtt_ms] {
            if (nack_requester_) {
                nack_requester_->UpdateRtt(rtt_ms);
            }
        });
        // Тот же замер нужен буферу показа: по нему оценка джиттера отделяет время круга
        // повтора от разброса сети.
        if (playout_task_queue_) {
            playout_task_queue_->PostTask([this, rtt_ms] {
                if (buffer_controller_) {
                    buffer_controller_->UpdateRtt(rtt_ms);
                }
            });
        }
    }

    /// Проход туда-обратно, измеренный ЭТОЙ сессией. -1 — ещё не измеряли.
    OmniWebRtcMediaStatus TransportRttUs(int64_t* out_rtt_us)
    {
        if (out_rtt_us == nullptr) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }
        std::lock_guard<std::mutex> lock(mutex_);
        *out_rtt_us = transport_rtt_us_;
        return OMNI_WEBRTC_MEDIA_STATUS_OK;
    }

    /// Принять отчёт о доставке и отдать его GoogCC.
    ///
    /// Это и есть та связь, которой не было: до сих пор регулятор строил оценку по потерям и RTT,
    /// то есть узнавал о заторе по его последствиям. Теперь он получает, что и когда доехало.
    /// Принять отчёт о доставке от собеседника.
    ///
    /// #57 ШАГ C2b: САМ ОТЧЁТ МЫ БОЛЬШЕ НЕ ПРИМЕНЯЕМ.
    ///
    /// Он приходит настоящим RTCP и уходит сборщику через DeliverRtcp вместе со всем остальным
    /// RTCP — там его разбирает RTCPReceiver и отдаёт NetworkLinkRtcpObserver сборщика, то есть в
    /// его собственный GoogCC. Здесь остаётся ровно две вещи, которых сборщик наружу не отдаёт:
    /// счётчики (сколько отчётов пришло и о скольких пакетах) и ПРОХОД ТУДА-ОБРАТНО.
    ///
    /// Отдельный вход FFI сохранён потому, что оболочка везёт отчёт своим каналом, а не в общем
    /// потоке медиа. Когда обе половины поедут одним путём, он схлопнется в PushRtpPackets.
    OmniWebRtcMediaStatus ApplyTransportFeedback(const uint8_t* data, size_t len)
    {
        if (data == nullptr || len == 0) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }

        webrtc::TimeDelta measured_rtt = webrtc::TimeDelta::PlusInfinity();
        {
            std::lock_guard<std::mutex> lock(feedback_mutex_);
            const webrtc::Timestamp now = env_.clock().CurrentTime();
            size_t offset = 0;
            while (offset < len) {
                webrtc::rtcp::CommonHeader header;
                if (!header.Parse(data + offset, len - offset)) {
                    break;
                }
                webrtc::rtcp::CongestionControlFeedback feedback;
                if (feedback.Parse(header)) {
                    transport_feedback_packets_reported_ += feedback.packets().size();
                    ++transport_feedback_reports_applied_;
                    // МИНИМАЛЬНЫЙ ПРОХОД ТУДА-ОБРАТНО ПО ЭТОМУ ОТЧЁТУ.
                    //
                    // Минимум, а не среднее: минимальный проход устойчив к задержке самого
                    // отчёта, среднее её в себя вбирает. Пакеты, которых нет в нашем кольце
                    // времён отправки, пропускаются.
                    for (const auto& packet_info : feedback.packets()) {
                        // Только наш медиапоток: в кольце лежат его номера, а у повторов (RTX) свой
                        // номерной ряд — совпавший номер указал бы на чужое время отправки.
                        if (packet_info.ssrc != ssrc_) {
                            continue;
                        }
                        const auto at = sent_at_by_packet_id_.find(
                            sent_sequence_unwrapper_.PeekUnwrap(packet_info.sequence_number));
                        if (at == sent_at_by_packet_id_.end()) {
                            continue;
                        }
                        const webrtc::TimeDelta rtt = now - at->second;
                        if (rtt.IsFinite() && rtt >= webrtc::TimeDelta::Zero() &&
                            rtt < measured_rtt) {
                            measured_rtt = rtt;
                        }
                    }
                }
                offset += header.packet_size();
            }
        }

        // САМИ БАЙТЫ — СБОРЩИКУ. Это и есть применение отчёта: его GoogCC сопоставит их со своей
        // историей отправки и подвинет и оценку, и пейсер.
        if (rtp_task_queue_) {
            std::vector<uint8_t> bytes(data, data + len);
            rtp_task_queue_->PostTask([this, bytes = std::move(bytes)]() mutable {
                if (rtp_video_sender_) {
                    rtp_video_sender_->DeliverRtcp(bytes);
                }
            });
        }

        if (!measured_rtt.IsFinite()) {
            return OMNI_WEBRTC_MEDIA_STATUS_OK;
        }

        // Только для диагностики и выборки ABR: запросчику NACK приёмной половины RTT приходит
        // от её собственного модуля (OmniRttObserver), а не отсюда.
        std::lock_guard<std::mutex> lock(mutex_);
        transport_rtt_us_ = measured_rtt.us();
        return OMNI_WEBRTC_MEDIA_STATUS_OK;
    }

public:
    /// Куда отдавать исходящие пакеты этой сессии (RTP отправителя и RTCP приёмника).
    void SetPacketSink(OmniWebRtcMediaPacketSink sink, void* context)
    {
        packet_sink_.Set(sink, context);
    }

    /// Точки входа для OmniModuleTransport. Публичны потому, что транспорт живёт снаружи класса и
    /// зовёт их извне. Обе потокобезопасны — их зовут с очереди модуля, а не с той, что держит
    /// mutex_.
    ///
    /// Пакет уходит в ядро прямо отсюда, в момент, когда его выпустил пейсер. Счётчики — для
    /// диагностики и давности обратной связи (SR модуль строит сам).
    bool SendModuleRtpPacket(std::span<const uint8_t> packet)
    {
        module_rtp_sent_.fetch_add(1, std::memory_order_relaxed);
        if (!packet_sink_.Send(packet.data(), packet.size())) {
            return false;
        }
        rtp_packets_sent_.fetch_add(1, std::memory_order_relaxed);
        rtp_octets_sent_.fetch_add(packet.size(), std::memory_order_relaxed);
        return true;
    }

    /// RTCP приёмника — мимо пейсера, как у апстрима (`Transport::SendRtcp` идёт прямо в сокет).
    void SendOutgoingRtcp(const uint8_t* data, size_t size)
    {
        if (packet_sink_.Send(data, size)) {
            rtcp_packets_sent_.fetch_add(1, std::memory_order_relaxed);
        }
    }



    // ApplyEncoderRates УБРАНА (#61, шаг D2b): скорость кодировщику назначает оркестратор,
    // а наш потолок доходит до него конфигурацией. Последний её вызов ушёл вместе с тактом ABR.


    // Wave 5 Phase 5a-step2 (2026-05-16): MaxPacingBudgetBytesUnsafe,
    // RefillPacingBudgetUnsafe and pace_budget_bytes_ removed — leaky-bucket
    // policy is now PacingController's responsibility.

    // HEVC: the frames libwebrtc's assembler hands over start at the IDR — the VPS/SPS/PPS that
    // travelled in front of it (single NAL or aggregation packet, RFC 7798) are not in the frame
    // (live 2026-10-13; the encoder's key frame was VPS,SPS,PPS,IDR). They are remembered here
    // and put back in front of a key frame that lacks them (DecodeAssembledFrame).
    void CacheH265ParameterSetsUnsafe(std::span<const uint8_t> payload)
    {
        if (payload.size() < 3) {
            return;
        }
        const auto remember = [this](const uint8_t* nal, size_t size) {
            if (size < 3) {
                return;
            }
            const uint8_t type = (nal[0] >> 1) & 0x3Fu;
            std::vector<uint8_t>* slot = type == 32u ? &h265_vps_ : type == 33u ? &h265_sps_
                                       : type == 34u ? &h265_pps_ : nullptr;
            if (slot != nullptr) {
                slot->assign(nal, nal + size);
            }
        };
        const uint8_t type = (payload[0] >> 1) & 0x3Fu;
        if (type == 48u) {  // aggregation packet: [2-byte header] then ([2-byte size][NAL])*
            size_t offset = 2;
            while (offset + 2 <= payload.size()) {
                const size_t size = (static_cast<size_t>(payload[offset]) << 8) | payload[offset + 1];
                offset += 2;
                if (offset + size > payload.size()) {
                    break;
                }
                remember(payload.data() + offset, size);
                offset += size;
            }
        } else {
            remember(payload.data(), payload.size());
        }
    }

    // Put the remembered VPS/SPS/PPS in front of an HEVC key frame that does not carry them.
    bool PrependH265ParameterSets(webrtc::EncodedImage& image)
    {
        std::vector<uint8_t> prefix;
        {
            std::lock_guard<std::mutex> lock(mutex_);
            if (h265_vps_.empty() || h265_sps_.empty() || h265_pps_.empty()) {
                return false;
            }
            for (const std::vector<uint8_t>* set : {&h265_vps_, &h265_sps_, &h265_pps_}) {
                prefix.insert(prefix.end(), {0, 0, 0, 1});
                prefix.insert(prefix.end(), set->begin(), set->end());
            }
        }
        for (const auto& nalu : webrtc::H264::FindNaluIndices(
                 std::span<const uint8_t>(image.data(), image.size()))) {
            if (nalu.payload_size > 0 && ((image.data()[nalu.payload_start_offset] >> 1) & 0x3Fu) == 32u) {
                return false;  // already there
            }
        }
        prefix.insert(prefix.end(), image.data(), image.data() + image.size());
        image.SetEncodedData(webrtc::EncodedImageBuffer::Create(prefix.data(), prefix.size()));
        ++h265_parameter_set_prepends_;
        return true;
    }

    void CacheReceivedRtpPayloadUnsafe(const webrtc::RtpPacketReceived& packet)
    {
        std::span<const uint8_t> payload = packet.PayloadBuffer();
        if (payload.empty()) {
            return;
        }

        const uint16_t sequence = packet.SequenceNumber();
        std::vector<uint8_t> payload_copy(payload.begin(), payload.end());
        auto previous = received_rtp_payload_cache_.find(sequence);
        if (previous != received_rtp_payload_cache_.end()) {
            received_rtp_payload_cache_bytes_ -= previous->second.size();
            previous->second = std::move(payload_copy);
        } else {
            received_rtp_payload_cache_.emplace(sequence, std::move(payload_copy));
            received_rtp_payload_order_.push_back(sequence);
        }

        received_rtp_payload_cache_bytes_ += received_rtp_payload_cache_[sequence].size();
        while ((!received_rtp_payload_order_.empty()) &&
               (received_rtp_payload_cache_.size() > kMaxReceivedRtpPayloadCachePackets ||
                received_rtp_payload_cache_bytes_ > kMaxReceivedRtpPayloadCacheBytes)) {
            const uint16_t stale_sequence = received_rtp_payload_order_.front();
            received_rtp_payload_order_.pop_front();
            auto stale = received_rtp_payload_cache_.find(stale_sequence);
            if (stale == received_rtp_payload_cache_.end()) {
                continue;
            }

            received_rtp_payload_cache_bytes_ -= stale->second.size();
            received_rtp_payload_cache_.erase(stale);
        }
    }

    static bool AppendH264RtpPayloadAsAnnexB(
        const std::vector<uint8_t>& payload,
        std::vector<uint8_t>& annex_b)
    {
        if (payload.empty()) {
            return false;
        }

        const uint8_t nalu_type = payload[0] & 0x1Fu;
        if (nalu_type > 0 && nalu_type < 24u) {
            AppendAnnexBStartCode(annex_b);
            annex_b.insert(annex_b.end(), payload.begin(), payload.end());
            return true;
        }

        if (nalu_type == 24u) {
            size_t offset = 1;
            while (offset + 2u <= payload.size()) {
                const uint16_t nalu_len = ReadBigEndian16(payload.data() + offset);
                offset += 2u;
                if (nalu_len == 0 || offset + nalu_len > payload.size()) {
                    return false;
                }

                AppendAnnexBStartCode(annex_b);
                annex_b.insert(
                    annex_b.end(),
                    payload.begin() + static_cast<ptrdiff_t>(offset),
                    payload.begin() + static_cast<ptrdiff_t>(offset + nalu_len));
                offset += nalu_len;
            }

            return offset == payload.size();
        }

        if (nalu_type == 28u) {
            if (payload.size() < 3u) {
                return false;
            }

            const uint8_t fu_indicator = payload[0];
            const uint8_t fu_header = payload[1];
            const bool start = (fu_header & 0x80u) != 0;
            const uint8_t original_nalu_header = (fu_indicator & 0xE0u) | (fu_header & 0x1Fu);
            if (start) {
                AppendAnnexBStartCode(annex_b);
                annex_b.push_back(original_nalu_header);
            }

            annex_b.insert(annex_b.end(), payload.begin() + 2, payload.end());
            return true;
        }

        return false;
    }

    bool BuildH264AnnexBFrameFromCacheUnsafe(
        uint16_t first_sequence,
        uint16_t last_sequence,
        std::vector<uint8_t>& annex_b) const
    {
        annex_b.clear();
        uint16_t sequence = first_sequence;
        for (size_t packet_count = 0; packet_count < kMaxReceivedRtpPayloadCachePackets; ++packet_count) {
            const auto payload = received_rtp_payload_cache_.find(sequence);
            if (payload == received_rtp_payload_cache_.end() ||
                !AppendH264RtpPayloadAsAnnexB(payload->second, annex_b)) {
                return false;
            }

            if (sequence == last_sequence) {
                return !annex_b.empty();
            }

            sequence = static_cast<uint16_t>(sequence + 1u);
        }

        return false;
    }

    static void AppendAnnexBStartCode(std::vector<uint8_t>& target)
    {
        target.insert(target.end(), {0, 0, 0, 1});
    }

    static bool AnnexBContainsH264NaluType(const std::vector<uint8_t>& data, uint8_t expected_type)
    {
        for (const auto& nalu : webrtc::H264::FindNaluIndices(data)) {
            if (nalu.payload_size == 0) {
                continue;
            }

            if ((data[nalu.payload_start_offset] & 0x1Fu) == expected_type) {
                return true;
            }
        }

        return false;
    }

    bool ConsumePendingKeyframeRequest()
    {
        std::lock_guard<std::mutex> lock(mutex_);
        const bool requested = force_next_keyframe_;
        force_next_keyframe_ = false;
        return requested;
    }

    uint32_t HighestExtendedSequenceUnsafe() const
    {
        return has_receiver_sequence_ ? receiver_sequence_cycles_ + receiver_highest_sequence_ : 0;
    }

    /// ПОТЕРИ И ДЖИТТЕР СЧИТАЕТ АПСТРИМ — ЗДЕСЬ ТОЛЬКО ЧТЕНИЕ.
    ///
    /// Тут стоял свой счёт: «получено» росло лишь на пакете, который двигает старший номер. Любой
    /// пакет, пришедший не по порядку, и любой восстановленный повтором (он всегда позже соседей)
    /// навсегда числился потерянным. На TCP-двери порядок идеальный — 0 %; на реальном UDP та же
    /// пара рапортовала 12–33 % при ~1 % на проводе (живьём 2026-09-24). Апстримовский
    /// StreamStatisticianImpl::UpdateCounters уменьшает потери на КАЖДЫЙ принятый пакет, в каком
    /// бы порядке он ни пришёл, — и уже кормит настоящие RTCP-отчёты. Два счёта одной величины
    /// разошлись; остался один. См. docs/bugs/datagrams-we-counted-as-sent-never-left.md.
    /// Решения регулятора из его собственного журнала (OmniBweEventLog), полями диагностики.
    std::string BweDiagnosticsJsonFields() const
    {
        const OmniBweEventLog::Snapshot bwe = bwe_event_log_.snapshot();
        std::ostringstream out;
        out << R"(,"bwe_delay_bps":)" << bwe.delay_bps
            << R"(,"bwe_delay_state":)" << bwe.delay_state
            << R"(,"bwe_delay_updates":)" << bwe.delay_updates
            << R"(,"bwe_delay_overuse_updates":)" << bwe.delay_overuse_updates
            << R"(,"bwe_loss_bps":)" << bwe.loss_bps
            << R"(,"bwe_loss_fraction":)" << static_cast<int>(bwe.loss_fraction)
            << R"(,"bwe_loss_updates":)" << bwe.loss_updates
            << R"(,"bwe_probes_created":)" << bwe.probes_created
            << R"(,"bwe_probes_succeeded":)" << bwe.probes_succeeded
            << R"(,"bwe_probe_last_success_bps":)" << bwe.probe_last_success_bps
            << R"(,"bwe_probes_failed":)" << bwe.probes_failed
            << R"(,"bwe_probe_last_failure_reason":)" << bwe.probe_last_failure_reason;
        return out.str();
    }

    webrtc::RtpReceiveStats ReceiverStatsUnsafe() const
    {
        const webrtc::StreamStatistician* statistician =
            receive_statistics_ && receiver_ssrc_ != 0
                ? receive_statistics_->GetStatistician(receiver_ssrc_)
                : nullptr;
        return statistician ? statistician->GetStats() : webrtc::RtpReceiveStats{};
    }

    // Wave 5b (2026-05-18): Gap detection / missing-packet population
    // was removed — NackRequester::OnReceivedPacket detects gaps and runs
    // its own RTT-aware retry ladder. This function now only tracks
    // sequence cycles for the RR extended-sequence-number computation.
    // The kReasonMissingRtpPackets keyframe-on-burst safeguard is gone
    // too because NackRequester escalates to KeyFrameRequestSender (and
    // through it to RequestReceiverKeyframeUnsafe via the adapter) when
    // its retry budget is exhausted.
    bool TrackReceiverSequenceUnsafe(uint16_t sequence, int64_t /*now_ms*/)
    {
        if (!has_receiver_sequence_) {
            has_receiver_sequence_ = true;
            receiver_highest_sequence_ = sequence;
            return true;
        }

        const int16_t delta = static_cast<int16_t>(sequence - receiver_highest_sequence_);
        if (delta <= 0) {
            return false;
        }

        if (sequence < receiver_highest_sequence_) {
            receiver_sequence_cycles_ += 1u << 16u;
        }

        receiver_highest_sequence_ = sequence;
        return true;
    }

    void TrackReceiverPacketUnsafe(const webrtc::RtpPacketReceived& packet, int64_t arrival_ms)
    {
        const bool first_packet_of_stream = receiver_ssrc_ == 0;
        receiver_ssrc_ = packet.Ssrc();
        if (first_packet_of_stream) {
            // Ask for a keyframe the moment this decoder sees its first packet. It joins the
            // stream mid-flight — the shell builds it lazily on the peer's first video packet —
            // so what it is holding is a P-frame referencing state it never had.
            //
            // This used to be covered by accident: the encoder emitted an IDR every 2 s, so a
            // fresh decoder just waited for the next one. That periodic IDR is gone, which makes
            // asking explicit work that has to be done here. Without it the first picture waits
            // for a decode failure to escalate into a PLI.
            RequestReceiverKeyframeUnsafe(arrival_ms, kReasonDecoderStarted);
            // Start the watchdog clock from the stream's first packet, not from zero — otherwise
            // it would fire on the very first packet, before anything COULD have assembled.
            last_assembled_frame_ms_ = arrival_ms;
        } else if (last_assembled_frame_ms_ != 0 &&
                   arrival_ms - last_assembled_frame_ms_ >= kNoAssembledFrameTimeoutMs) {
            ++no_assembled_watchdog_fired_;
            // Packets are arriving — we are literally inside the per-packet path — and yet nothing
            // has assembled for seconds. Nobody else will notice: no assembled frame means no
            // decode to fail and no playout to expire. Ask, and keep asking.
            RequestReceiverKeyframeUnsafe(arrival_ms, kReasonNoAssembledFrames);
        }
        const uint16_t sequence = packet.SequenceNumber();
        const bool advances_highest_sequence = TrackReceiverSequenceUnsafe(sequence, arrival_ms);
        // Сеть переставила пакеты (повтор не в счёт — он опаздывает по построению). Не потеря,
        // но видимая величина: именно она раздувала прежний самодельный счёт потерь.
        if (!advances_highest_sequence && !packet.recovered()) {
            ++receiver_out_of_order_packets_;
        }
        // NackRequester видит каждый принятый пакет и сам считает, каких номеров не хватает.
        // Посылается задачей на его очередь (он на sequence-checker'е), потому что здесь
        // держится mutex_.
        //
        // ПРИЗНАК «ВОССТАНОВЛЕН» ОБЯЗАТЕЛЕН, И ДО #53 ЕГО ЗДЕСЬ НЕ БЫЛО.
        //
        // Пока повторы приходили обычными пакетами, разницы не было. С переходом на RTX она
        // появилась: пакет, развёрнутый из RTX, — это НЕ доказательство того, что канал
        // донёс всё до его номера, а именно так его и читал запросчик. У апстрима для таких
        // пакетов отдельная ветка (`nack_requester.cc:190`): он кладёт номер в
        // recovered_list_ и НЕ просит повторить пропущенное перед ним, тогда как обычный
        // путь зовёт AddPacketsToNack(newest+1, seq) и двигает newest_seq_num_ вперёд.
        //
        // То есть без этого признака восстановленный пакет мог объявить полученным всё, что
        // между ним и прошлым максимумом, — и запросить повтор того, что ещё едет.
        const bool recovered = packet.recovered();
        if (adaptation_task_queue_) {
            adaptation_task_queue_->PostTask([this, sequence, recovered] {
                if (nack_requester_) {
                    nack_requester_->OnReceivedPacket(sequence, recovered);
                }
            });
        }
    }

    /// ПАРАМЕТР `force` УБРАН ВМЕСТЕ С ЕДИНСТВЕННЫМ ЕГО ПОТРЕБИТЕЛЕМ (#60).
    ///
    /// Он появился ради случая «мы сами выбросили опорный кадр по возрасту, и проглоченная
    /// просьба гасит картинку до конца звонка». Выброса по возрасту больше нет: показ планирует
    /// апстримовский буфер, а он декодируемую единицу не выбрасывает. Оставлять параметр без
    /// вызывающего значило бы хранить рычаг от снятого механизма.
    void RequestReceiverKeyframeUnsafe(int64_t now_ms, uint32_t reason_code)
    {
        if (receiver_ssrc_ == 0 || now_ms - last_keyframe_request_ms_ < kKeyframeRequestIntervalMs) {
            return;
        }

        last_keyframe_request_ms_ = now_ms;
        last_keyframe_request_reason_ = reason_code;
        // PLI уходит приёмным модулем, как у апстрима (RtpVideoStreamReceiver2::RequestKeyFrame).
        // Очередь та же, что у включения отчётов на первом пакете, поэтому просьба «декодер
        // стартовал» не обгоняет включение и не теряется на выключенном модуле.
        if (rtp_task_queue_) {
            rtp_task_queue_->PostTask([this] {
                if (receiver_rtcp_module_) {
                    receiver_rtcp_module_->SendPictureLossIndication();
                }
            });
        }
    }

    /// Включить отчёты приёмника — один раз, на первом пришедшем медиапакете.
    ///
    /// До него докладывать не о чем, а молчать обязательно: см. SetRTCPStatus(kOff) при создании.
    void EnableReceiverRtcpOnce()
    {
        bool expected = false;
        if (!receiver_rtcp_enabled_.compare_exchange_strong(expected, true,
                                                            std::memory_order_relaxed)) {
            return;
        }
        if (!rtp_task_queue_) {
            return;
        }
        rtp_task_queue_->PostTask([this] {
            if (receiver_rtcp_module_) {
                // kCompound, а не kReducedSize: сокращённый вид требует согласования, а у нас его
                // нет.
                receiver_rtcp_module_->SetRTCPStatus(webrtc::RtcpMode::kCompound);
            }
        });
    }

    /// Номера, которые NackRequester решил переспросить, — настоящим RTCP приёмного модуля, как у
    /// апстрима (RtpVideoStreamReceiver2::SendNack). Зовётся с очереди запросчика; модуль живёт
    /// на rtp-очереди, туда и ставится задача.
    ///
    /// Длинный список RTCPSender режет сам по max_packet_size — предел датаграммы ему задан при
    /// создании (см. SetMaxRtpPacketSize у приёмного модуля).
    void SendReceiverNack(const std::vector<uint16_t>& sequence_numbers)
    {
        if (!rtp_task_queue_) {
            return;
        }
        rtp_task_queue_->PostTask([this, sequence_numbers] {
            if (receiver_rtcp_module_) {
                receiver_rtcp_module_->SendNack(sequence_numbers);
            }
        });
    }

    // Wave 5b (2026-05-18): keyframe-request callback from NackRequester
    // (via OmniKeyFrameRequestSenderAdapter). Fires on adaptation_task_queue_
    // when libwebrtc decides packet loss is unrecoverable via NACK retries
    // and a full keyframe is needed. We just route into our existing
    // RequestReceiverKeyframeUnsafe path, which respects the per-stream
    // request interval throttle.
    void OnNackRequesterAsksForKeyframe()
    {
        std::lock_guard<std::mutex> lock(mutex_);
        RequestReceiverKeyframeUnsafe(MonotonicNowMs(), kReasonNackEscalation);
    }

    OmniWebRtcMediaStatus PushRtpPacket(const uint8_t* packet, size_t packet_len)
    {
        if (packet == nullptr || packet_len > max_rtp_packet_bytes_) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }

        // РАСПОЗНАВАНИЕ RTCP ИДЁТ ДО ПРОВЕРКИ ДЛИНЫ RTP, И ЭТО НЕ ПРИДИРКА.
        //
        // У RTP заголовок 12 байт, у RTCP — 4, и пустой отчёт приёмника короче любого
        // RTP-пакета. Стояла проверка длины первой — и все короткие отчёты отвергались с
        // INVALID_ARGUMENT, а стенд показа экрана падал на «link pump failed». Порядок
        // проверок здесь — часть протокола, а не оформление.

        // RTCP ЕДЕТ В ТОЙ ЖЕ ТРУБЕ И РАЗБИРАЕТСЯ ЗДЕСЬ (#57, шаг A).
        //
        // Отчёты приёмника, NACK и PLI приходят теперь настоящими байтами RTCP. Отдаём их
        // отправляющему модулю: он сам достанет из них потери, джиттер и, ГЛАВНОЕ, RTT — величину,
        // которой у нас до сих пор просто не было (rtt_ms() возвращал ноль, из-за чего
        // ExpectedRetransmissionTime отдавал умолчание, а NackRequester получал ноль и
        // переспрашивал непрерывно).
        if (LooksLikeRtcp(packet, packet_len)) {
            rtcp_packets_received_.fetch_add(1, std::memory_order_relaxed);
            // #57 ШАГ C2b: RTCP УХОДИТ СБОРЩИКУ ЦЕЛИКОМ.
            //
            // DeliverRtcp отдаёт байты модулю сборщика, а тот раздаёт разобранное по местам сам:
            // NACK -> история повторов, RR -> потери и RTT, отчёт о доставке RFC 8888 ->
            // NetworkLinkRtcpObserver, которым RtpVideoSender назначает сам сборщик
            // (rtp_video_sender.cc:245). То есть весь круг обратной связи замыкается одной
            // строкой, и ни одного провода мы больше не ставим руками.
            //
            // И ПРИЁМНОМУ МОДУЛЮ ТОЖЕ — как Call::DeliverRtcpPacket раздаёт пакет всем потокам
            // сразу, а каждый берёт своё по SSRC. Приёмному нужен отчёт отправителя собеседника:
            // без него в нашем RR нет LSR/DLSR, и отправитель не может вычислить по нему RTT.
            if (rtp_task_queue_) {
                std::vector<uint8_t> bytes(packet, packet + packet_len);
                rtp_task_queue_->PostTask([this, bytes = std::move(bytes)]() mutable {
                    if (rtp_video_sender_) {
                        rtp_video_sender_->DeliverRtcp(bytes);
                    }
                    if (receiver_rtcp_module_) {
                        receiver_rtcp_module_->IncomingRtcpPacket(bytes);
                    }
                });
            }
            return OMNI_WEBRTC_MEDIA_STATUS_OK;
        }

        if (packet_len < kRtpHeaderBytes) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }

        // С ТОЙ ЖЕ картой расширений, что и у отправителя: иначе структура кадров, аккуратно
        // записанная в заголовок, будет молча не прочитана, и приёмник вернётся к линейной цепочке.
        webrtc::RtpPacketReceived received(&VideoRtpExtensionMap());
        if (!received.Parse(packet, packet_len)) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }
        // ЧТО ЭТО ЗА ПАКЕТ: наш видеопоток, наш поток RTX или вообще не наш.
        //
        // Оболочка веером раздаёт каждую входящую датаграмму И видеосессии, И звуковой — поэтому
        // «не наше» здесь не ошибка, а обычное дело, и молча отбрасывается. Возврат
        // INVALID_ARGUMENT отсюда однажды рвал весь входящий пакет разом, как только начинался
        // звук, и вместе со звуком ложилось видео.
        const bool is_rtx = received.Ssrc() == kOmniVideoRtxSsrc &&
                            received.PayloadType() == kOmniVideoRtxPayloadType;
        const bool is_media = received.PayloadType() == NativeVideoCodecPayloadType(codec_kind_) &&
                              received.payload_size() != 0;
        if (!is_rtx && !is_media) {
            return OMNI_WEBRTC_MEDIA_STATUS_OK;
        }

        // ВРЕМЯ ПРИБЫТИЯ И ОТЧЁТ О ДОСТАВКЕ — ДО ВСЯКОГО ОТСЕВА.
        //
        // Оценка канала интересуется ПАКЕТАМИ, а не тем, сложились ли они в кадр и несут ли вообще
        // изображение. Пакет добивки не несёт ничего — и именно его прибытие показывает каналу его
        // собственную ёмкость. Отсей его раньше отчёта, и пробы станут неизмеримы: отправитель
        // будет слать их и не узнавать результата. Ровно поэтому эта часть стоит выше развилки, а
        // не внутри ветки видео.
        //
        // ВРЕМЯ ПРИБЫТИЯ — МОМЕНТ ВЫБОРКИ ОБОЛОЧКОЙ, И ЭТО ИЗВЕСТНОЕ ОТСТУПЛЕНИЕ ОТ КОНТРАКТА.
        //
        // libwebrtc ждёт время прихода из сети. Оболочка выгребает приём раз в ~15.6 мс, так что
        // пакеты одной выборки получают одно время, и часть проб GoogCC отвергается
        // (kInvalidSendReceiveInterval/Ratio). Время прихода из сети пробовали 2026-09-25 (возраст из
        // ядра через FFI): отказы проб ушли, цель не выросла, а рывки на Wi-Fi-пути стали хуже
        // (наибольший разрыв показа 172–953 мс против 78–250, 12 прогонов чередованием, p≈0.01).
        // Откачено до понимания, ЧТО в регуляторе делает честные метки хуже. Разбор —
        // docs/bugs/datagrams-we-counted-as-sent-never-left.md §«Время прихода».
        received.set_arrival_time(env_.clock().CurrentTime());
        if (feedback_generator_) {
            std::lock_guard<std::mutex> lock(feedback_mutex_);
            feedback_generator_->OnReceivedPacket(received);
            (void)feedback_generator_->Process(env_.clock().CurrentTime());
        }

        if (is_rtx) {
            ++rtx_packets_received_;
            if (!rtx_receive_stream_) {
                return OMNI_WEBRTC_MEDIA_STATUS_OK;
            }
            // Дальше — либо добивка (RtxReceiveStream отбросит её сам), либо восстановленный
            // медиапакет, который приедет обратно в HandleMediaRtpPacket через OmniRtxMediaSink.
            rtx_receive_stream_->OnRtpPacket(received);
            return OMNI_WEBRTC_MEDIA_STATUS_OK;
        }

        return HandleMediaRtpPacket(received);
    }

public:
    /// Обработать медиапакет — пришедший напрямую или восстановленный из RTX.
    ///
    /// Публично потому, что зовётся снаружи из OmniRtxMediaSink, тем же приёмом, что и точки входа
    /// транспорта. Отчёт о доставке здесь НЕ трогается: восстановленный пакет уже отчитан выше,
    /// под своим номером в потоке RTX, и отчитать его второй раз значило бы соврать о доставке.
    OmniWebRtcMediaStatus HandleMediaRtpPacket(webrtc::RtpPacketReceived& received)
    {
        if (received.recovered()) {
            ++rtx_packets_recovered_;
            // ЗАПОМИНАЕМ, ЧЕЙ КАДР ДОСТРОЕН ПОВТОРОМ.
            //
            // Буфер показа спрашивает у кадра delayed_by_retransmission() и по ответу решает,
            // кормить ли оценку джиттера межкадровым интервалом. Интервал кадра, дождавшегося
            // повтора, — это время круга ретрансмиссии, а не разброс сети; принять одно за другое
            // значит раздуть задержку показа на ровном месте. Сборщик кадров этого признака не
            // несёт (times_nacked у него всегда 0), а мы его знаем.
            std::lock_guard<std::mutex> lock(mutex_);
            retransmitted_frame_timestamps_.insert(received.Timestamp());
            while (retransmitted_frame_timestamps_.size() > kMaxRetransmittedTimestamps) {
                retransmitted_frame_timestamps_.erase(retransmitted_frame_timestamps_.begin());
            }
        }

        // ПРОВЕРКА, КОТОРАЯ МОЖЕТ ПРОВАЛИТЬСЯ.
        //
        // Самая дорогая ошибка переворота #50 — молчаливая. Если у отправителя не зарегистрировать
        // RtpGenericFrameDescriptorExtension00, RTPSenderVideo просто не станет писать описатель;
        // приёмник вернётся к линейной цепочке зависимостей, кадры продолжат собираться и
        // декодироваться, набор останется зелёным — и ни один прежний счётчик этого не покажет.
        //
        // Поэтому факт считается прямо здесь, на проводе: сколько пакетов принесли описатель и у
        // скольких из них внутри были зависимости. Ноль в первом значит, что структуры на проводе
        // нет вовсе; ноль во втором при ненулевом первом — что описатель есть, а зависимости в него
        // не попали. Обе величины отличают «работает» от «выглядит работающим», чего не делает ни
        // psnr, ни число собранных кадров.
        webrtc::RtpGenericFrameDescriptor inbound_descriptor;
        if (received.GetExtension<webrtc::RtpGenericFrameDescriptorExtension00>(
                &inbound_descriptor)) {
            ++received_generic_descriptors_;
            if (!inbound_descriptor.FrameDependenciesDiffs().empty()) {
                ++received_generic_dependencies_;
            }
        }

        received.set_payload_type_frequency(kRtpClockHz);
        if (!assembler_) {
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }

        // ВРЕМЯ ПРИБЫТИЯ проставлено выше, до развилки. Без него отчёт о доставке — пустая
        // формальность: оно не проставлялось вовсе, и это было невидимо, потому что отчёты
        // собирались, доезжали и сопоставлялись все до единого (239 из 239), а оценки канала не
        // давали ни одной — внутри у них не было того единственного, ради чего их шлют.
        // Урок тот же, что и с сопоставлением: «вызов состоялся» и «вызов что-то дал» — разные
        // утверждения, и проверять надо второе.
        // Потери и джиттер для блоков отчёта считает апстрим, а не мы. Кормится КАЖДЫМ
        // медиапакетом, включая восстановленные из RTX — приёмник отчитывается о том, что реально
        // получил.
        // ВОССТАНОВЛЕННЫЕ ИЗ RTX В СТАТИСТИКУ НЕ ИДУТ — ТАК ДЕЛАЕТ АПСТРИМ.
        //
        // У RtxReceiveStream есть отдельный параметр rtp_receive_statistics, и туда он отдаёт
        // САМ RTX-ПАКЕТ (rtx_receive_stream.cc:78), а не развёрнутый из него медиапакет. Разница
        // не косметическая: у восстановленного пакета СТАРЫЙ номер и СТАРАЯ метка времени, и
        // статистика, приняв его за свежий, портит и долю потерь, и джиттер — то есть ровно те
        // две величины, ради которых отчёт и шлётся. Отправитель потом реагирует на выдумку.
        if (receive_statistics_ && !received.recovered()) {
            receive_statistics_->OnRtpPacket(received);
        }
        EnableReceiverRtcpOnce();

        const int64_t now_ms = MonotonicNowMs();
        webrtc::RtpVideoFrameAssembler::FrameVector assembled_frames = assembler_->InsertPacket(received);

        {
            std::lock_guard<std::mutex> lock(mutex_);
            TrackReceiverPacketUnsafe(received, now_ms);
            if (codec_kind_ == NativeVideoCodec::H264) {
                CacheReceivedRtpPayloadUnsafe(received);
            } else if (codec_kind_ == NativeVideoCodec::H265) {
                CacheH265ParameterSetsUnsafe(received.payload());
            }
            ++rtp_packets_received_;
        }

        rtp_frames_assembled_ += assembled_frames.size();
        for (webrtc::RtpVideoFrameAssembler::AssembledFrame& assembled_frame : assembled_frames) {
            SubmitAssembledFrame(assembled_frame.ExtractFrame());
        }

        return OMNI_WEBRTC_MEDIA_STATUS_OK;
    }

private:
    /// Отдать собранный кадр буферу показа.
    ///
    /// Здесь были QueueAssembledFrame и DecodeDuePlayoutFrames — наша обвязка вокруг FrameBuffer:
    /// свой расчёт целевой задержки по JitterEstimator, свой выброс «слишком старой» единицы по
    /// возрасту и свой сторож. Все три дефекта #56 жили в ней. Теперь связку держит
    /// VideoStreamBufferController, и от нас требуется ровно две вещи: класть кадры и забирать
    /// назначенные к показу.
    void SubmitAssembledFrame(std::unique_ptr<webrtc::EncodedFrame> frame)
    {
        if (!frame || frame->data() == nullptr || frame->size() == 0) {
            return;
        }

        // ВРЕМЯ БЕРЁТСЯ У ЧАСОВ ОКРУЖЕНИЯ, А НЕ У MonotonicNowMs.
        //
        // Контроллер сравнивает время прибытия кадра со СВОИМИ часами (env_.clock()). Под
        // виртуальными часами оба источника совпадают, но в продукте это разные эпохи:
        // MonotonicNowMs идёт от steady_clock, а часы апстрима — от своего монотонного счётчика.
        // Смешать их значит получить время показа, отличающееся от настоящего на случайную
        // константу, и сброс модуля времени на каждом кадре.
        const webrtc::Timestamp received_at = env_.clock().CurrentTime();
        const uint32_t rtp_timestamp = frame->RtpTimestamp();

        bool delayed_by_retransmission = false;
        {
            std::lock_guard<std::mutex> lock(mutex_);
            const auto it = retransmitted_frame_timestamps_.find(rtp_timestamp);
            if (it != retransmitted_frame_timestamps_.end()) {
                delayed_by_retransmission = true;
                retransmitted_frame_timestamps_.erase(it);
            }
            // Сборщик отдал кадр — приёмник не слеп. Отмечается ДО буфера показа намеренно:
            // сборка состоялась независимо от того, что показ решит дальше.
            const int64_t now_ms = received_at.ms();
            if (last_assembled_frame_ms_ != 0) {
                max_assembly_gap_ms_ =
                    std::max(max_assembly_gap_ms_, now_ms - last_assembled_frame_ms_);
            }
            last_assembled_frame_ms_ = now_ms;
        }

        // Приведение к RtpFrameObject здесь ЗАКОННО: кадр только что вышел из
        // RtpVideoFrameAssembler, который других типов не производит. Дальше по пути этого уже
        // нельзя — см. комментарий у OmniAssembledFrame.
        auto* rtp_frame = static_cast<webrtc::RtpFrameObject*>(frame.get());
        auto wrapped = std::make_unique<OmniAssembledFrame>(
            *rtp_frame, received_at, delayed_by_retransmission);
        frame.reset();

        if (!playout_task_queue_) {
            return;
        }
        playout_task_queue_->PostTask(
            [this, inserted = std::move(wrapped)]() mutable {
                if (!buffer_controller_) {
                    return;
                }
                buffer_controller_->InsertFrame(std::move(inserted));
                // Глубина буфера снимается ЗДЕСЬ, потому что спрашивать её можно только на
                // этой очереди: у контроллера внутри проверка последовательности.
                playout_pending_frames_.store(
                    static_cast<uint64_t>(std::max(0, buffer_controller_->Size())),
                    std::memory_order_relaxed);
                // ПУСК — НА ПЕРВОМ ЖЕ КАДРЕ, А НЕ ПРИ СОЗДАНИИ СЕССИИ.
                //
                // Апстрим заводит контроллер в Start(), потому что у него приёмный поток —
                // отдельный объект. У нас сессия одна на обе половины, и в сессии, которая только
                // отправляет, запущенный сторож декодируемости тикал бы каждые 200 мс до конца
                // звонка и просил бы ключевые кадры у самого себя.
                if (!playout_started_.exchange(true, std::memory_order_relaxed)) {
                    buffer_controller_->StartNextDecode(/*keyframe_required=*/true);
                }
            });
    }

public:
    /// Отдать оценку канала отправителю видео и кодировщику. Зовётся с rtp-очереди.
    void ApplyTargetRateToEncoder(webrtc::TargetTransferRate rate)
    {
        ++encoder_target_updates_;
        if (!rtp_video_sender_ || !video_stream_encoder_) {
            return;
        }
        webrtc::BitrateAllocationUpdate update;
        update.target_bitrate = rate.target_rate;
        update.packet_loss_ratio = rate.network_estimate.loss_rate_ratio;
        update.round_trip_time = rate.network_estimate.round_trip_time;
        update.cwnd_reduce_ratio = rate.cwnd_reduce_ratio;
        update.bwe_period = rate.network_estimate.bwe_period;

        const int framerate = encoder_stats_observer_
            ? std::max(1, encoder_stats_observer_->GetInputFrameRate())
            : fps_;
        rtp_video_sender_->OnBitrateUpdated(update, framerate);

        // Порядок и величины взяты из VideoSendStreamImpl::OnBitrateUpdated: сперва цель получает
        // отправитель видео (он делит её между полезной нагрузкой и защитой), и только ПОТОМ
        // кодировщик получает ту часть, которая действительно ему причитается.
        const webrtc::DataRate encoder_target =
            webrtc::DataRate::BitsPerSec(rtp_video_sender_->GetPayloadBitrateBps());
        const webrtc::DataRate protection =
            webrtc::DataRate::BitsPerSec(rtp_video_sender_->GetProtectionBitrateBps());
        webrtc::DataRate link_allocation = encoder_target > protection
            ? encoder_target - protection
            : webrtc::DataRate::Zero();
        link_allocation = std::max(encoder_target, link_allocation);

        encoder_last_target_bps_ = encoder_target.bps();
        video_stream_encoder_->OnBitrateUpdated(
            encoder_target,
            link_allocation,
            static_cast<uint8_t>(std::clamp<int>(
                static_cast<int>(update.packet_loss_ratio * 256), 0, 255)),
            update.round_trip_time.ms(),
            update.cwnd_reduce_ratio);
    }

    /// Конфигурация для оркестратора кодирования.
    ///
    /// Фабрику потоков НЕ ставим: она нужна только тестам, а без неё апстрим берёт свою
    /// EncoderStreamFactory — ту же, что и в живом WebRTC. `simulcast_layers` из одного слоя это
    /// и есть «не симулкаст»: у нас один поток и один SSRC.
    webrtc::VideoEncoderConfig BuildEncoderConfigUnsafe() const
    {
        webrtc::VideoEncoderConfig config;
        config.codec_type = NativeVideoCodecType(codec_kind_);
        config.video_format = webrtc::SdpVideoFormat(
            std::string(NativeVideoCodecName(codec_kind_)));
        config.content_type = screen_content_
            ? webrtc::VideoEncoderConfig::ContentType::kScreen
            : webrtc::VideoEncoderConfig::ContentType::kRealtimeVideo;
        config.number_of_streams = 1;
        // ПОТОЛОК — ЭТО РЕШЕНИЕ ABR, А НЕ СОГЛАСОВАННЫЙ ПРОФИЛЬ (#61, шаг D2b).
        //
        // Скорость кодировщику назначает регулятор канала внутри оркестратора. Единственный рычаг,
        // которым наша политика может его ограничить, — максимум в конфигурации; ровно так это и
        // устроено у апстрима, где максимум задаёт приложение. Поставь сюда согласованный профиль —
        // и решение ABR перестанет что-либо значить: замер показал цель 150 кбит/с при фактических
        // 786 тыс., потому что потолок стоял на профиле.
        //
        // Согласованный профиль остаётся АБСОЛЮТНОЙ границей: выше него мы не поднимаемся никогда.
        config.max_bitrate_bps = std::min(
            std::max(bitrate_bps_, kAbrMinBitrateBps), negotiated_bitrate_bps_);
        config.min_transmit_bitrate_bps = 0;
        // Пропуск кадров — механизм, которым кодировщик держит битрейт. Он же снимает нужду в
        // нашем ведре: аппаратный путь пропускать не умеет, и за него это делает оркестратор.
        config.frame_drop_enabled = true;
        // РАЗРЕШЕНИЕ АДАПТАЦИИ ПО КАЧЕСТВУ — ЭТО РЕШЕНИЕ ПРИЛОЖЕНИЯ (#61, шаг D3).
        //
        // Без него VideoStreamEncoderResourceManager не заводит НИ ОДИН из двух механизмов
        // изменения размера кадра — ни QualityScaler по QP, ни BandwidthQualityScaler по
        // фактическому выходу, — и оркестратор адаптирует только частоту по перегрузке
        // процессора. У апстрима это условие приложения (webrtc_video_engine.cc:2424): не
        // демонстрация экрана и ровно один активный поток. У нас потоков всегда один; про
        // демонстрацию экрана мы говорим прямо — см. screen_content_.
        config.is_quality_scaling_allowed = !screen_content_;

        webrtc::VideoStream stream;
        stream.width = static_cast<size_t>(width_);
        stream.height = static_cast<size_t>(height_);
        stream.max_framerate = fps_;
        stream.min_bitrate_bps = kAbrMinBitrateBps;
        stream.target_bitrate_bps = std::max(bitrate_bps_, kAbrMinBitrateBps);
        stream.max_bitrate_bps = config.max_bitrate_bps;
        stream.max_qp = 56;
        stream.active = true;
        config.simulcast_layers.push_back(std::move(stream));
        return config;
    }

    /// Оркестратор кодирования отдал кадр. Зовётся с его очереди кодирования.
    ///
    /// Здесь была PacketizeEncodedFrame: сборка RTPVideoHeader, номер кадра, временной слой,
    /// зависимость на последний опорный кадр, постановка в очередь модуля. Всё это теперь делают
    /// RtpPayloadParams и RtpVideoSender по CodecSpecificInfo, который даёт кодировщик. Нам
    /// осталось посчитать свои величины и передать кадр дальше.
    webrtc::EncodedImageCallback::Result OnEncodedImageReady(
        const webrtc::EncodedImage& encoded_image,
        const webrtc::CodecSpecificInfo* codec_specific_info)
    {
        if (encoded_image.data() == nullptr || encoded_image.size() == 0) {
            return webrtc::EncodedImageCallback::Result(
                webrtc::EncodedImageCallback::Result::OK);
        }

        ++frames_encoded_;

        // ЧТО КОДИРОВЩИК ОТДАЛ НА САМОМ ДЕЛЕ: габариты кадра, выход за пятисекундное окно и QP,
        // по которому судит QualityScaler (у аппаратного пути — разобранный из битстрима). Кадр тот
        // же, что апстрим отдаёт скейлеру следом за нами, так что здесь видно его вход без догадок:
        // «не просит уменьшить» отличается от «просит, и прав».
        {
            const int64_t now_ms = env_.clock().CurrentTime().ms();
            std::lock_guard<std::mutex> lock(encoded_window_mutex_);
            encoded_qp_.store(encoded_image.qp_, std::memory_order_relaxed);
            encoded_width_.store(static_cast<int>(encoded_image._encodedWidth),
                                 std::memory_order_relaxed);
            encoded_height_.store(static_cast<int>(encoded_image._encodedHeight),
                                  std::memory_order_relaxed);
            encoded_window_.push_back({now_ms, encoded_image.size()});
            encoded_window_bytes_ += encoded_image.size();
            while (!encoded_window_.empty() && now_ms - encoded_window_.front().at_ms > 5'000) {
                encoded_window_bytes_ -= encoded_window_.front().bytes;
                encoded_window_.pop_front();
            }
        }

        // СТРУКТУРА СЛОЁВ, ЧИТАЕМАЯ ИЗ САМОГО ПОТОКА: у кадра верхнего временного слоя
        // nal_ref_idc == 0, то есть на него никто не ссылается. Считаем долю здесь — в
        // единственной точке, через которую проходит выход кодировщика.
        const std::optional<uint8_t> nal_header = FirstSliceNalHeader(
            std::span<const uint8_t>(encoded_image.data(), encoded_image.size()));
        if (nal_header.has_value()) {
            if (((*nal_header >> 5) & 0x03u) != 0u) {
                ++frames_reference_;
            } else {
                ++frames_non_reference_;
            }
        }

        if (!rtp_video_sender_) {
            module_send_failures_.fetch_add(1, std::memory_order_relaxed);
            return webrtc::EncodedImageCallback::Result(
                webrtc::EncodedImageCallback::Result::ERROR_SEND_FAILED);
        }
        const webrtc::EncodedImageCallback::Result result =
            rtp_video_sender_->OnEncodedImage(encoded_image, codec_specific_info);
        if (result.error != webrtc::EncodedImageCallback::Result::OK) {
            module_send_failures_.fetch_add(1, std::memory_order_relaxed);
        }
        return result;
    }

    /// Оркестратор выбросил кадр, не дойдя до кодировщика.
    void OnEncoderDroppedFrame()
    {
        ++encoder_dropped_frames_;
    }

    /// Контроллер назначил кадр к показу. Зовётся на очереди показа.
    ///
    /// Декодировать ЗДЕСЬ нельзя: аппаратный декодер создан на устройстве рендерера, а рендерит
    /// поток оболочки. Поэтому кадр только откладывается, а декодирует его тот же поток, что и
    /// раньше, — ровно как у апстрима, где OnEncodedFrame перекладывает работу на decode_queue_.
    void OnPlayoutFrameReady(std::unique_ptr<webrtc::EncodedFrame> frame)
    {
        std::lock_guard<std::mutex> lock(pending_decode_mutex_);
        // Глубина 1 по построению: следующий кадр контроллер отдаст только после StartNextDecode,
        // а его зовёт поток-декодировщик, уже забрав предыдущий.
        pending_decode_frame_ = std::move(frame);
    }

    /// Декодировать нечего дольше положенного. Зовётся на очереди показа.
    void OnPlayoutDecodeTimeout(webrtc::TimeDelta wait)
    {
        (void)wait;
        decode_timeouts_.fetch_add(1, std::memory_order_relaxed);
        playout_keyframe_required_.store(true, std::memory_order_relaxed);
        {
            std::lock_guard<std::mutex> lock(mutex_);
            RequestReceiverKeyframeUnsafe(MonotonicNowMs(), kReasonDecodeTimeout);
        }
        // Контроллер на срабатывании останавливает сторож и ждёт, пока приёмная сторона снова
        // будет готова принять кадр. Сказать ему об этом — наша обязанность.
        if (buffer_controller_) {
            buffer_controller_->StartNextDecode(/*keyframe_required=*/true);
        }
    }

private:
    /// Декодировать кадр, назначенный к показу. Зовётся ТОЛЬКО с потока оболочки.
    ///
    /// Это роль апстримовской decode_queue_: здесь и декодирование, и вся работа с D3D11, и
    /// возврат управления контроллеру через StartNextDecode.
    void DecodePendingPlayoutFrame()
    {
        std::unique_ptr<webrtc::EncodedFrame> frame;
        {
            std::lock_guard<std::mutex> lock(pending_decode_mutex_);
            frame = std::move(pending_decode_frame_);
        }
        if (!frame) {
            return;
        }

        const bool keyframe_required = playout_keyframe_required_.load(std::memory_order_relaxed);
        bool decoded = false;
        if (keyframe_required && !frame->is_keyframe()) {
            // Ждём ключевой кадр: дельта-кадр декодировать не от чего. Считается отдельно, чтобы
            // «ждём опоры» не выглядело как «декодер сломался».
            ++playout_frames_awaiting_keyframe_;
        } else {
            const webrtc::Timestamp decode_start = env_.clock().CurrentTime();
            decoded = DecodeAssembledFrame(std::move(frame));
            const webrtc::Timestamp decode_end = env_.clock().CurrentTime();
            if (decoded && playout_timing_) {
                // Единственное, что модуль времени не может узнать сам: сколько заняло
                // декодирование. У апстрима это сообщает обратный вызов декодера
                // (generic_decoder.cc), у нас — эта строка.
                playout_timing_->StopDecodeTimer(decode_end - decode_start, decode_end);
            }
        }

        playout_keyframe_required_.store(!decoded, std::memory_order_relaxed);
        if (playout_task_queue_) {
            playout_task_queue_->PostTask([this, need_keyframe = !decoded] {
                if (buffer_controller_) {
                    buffer_controller_->StartNextDecode(need_keyframe);
                }
            });
        }
    }

    /// Декодировать кадр. Возвращает, добрался ли он до декодера.
    ///
    /// Ответ нужен вызывающему: по нему решается, требовать ли следующим ключевой кадр —
    /// ровно как в DecodeFrameResult::keyframe_required у апстрима. Раньше функция ничего не
    /// возвращала, и «декодировали» было неотличимо от «выбросили».
    bool DecodeAssembledFrame(std::unique_ptr<webrtc::EncodedFrame> frame)
    {
        if (!frame || frame->data() == nullptr || frame->size() == 0) {
            return false;
        }
        ++frames_fed_to_decoder_;

        webrtc::EncodedImage image = frame->EncodedImage();
        if (codec_kind_ == NativeVideoCodec::H265 &&
            image._frameType == webrtc::VideoFrameType::kVideoFrameKey) {
            (void)PrependH265ParameterSets(image);
        }
        if (codec_kind_ == NativeVideoCodec::H264) {
            // ПРИВЕДЕНИЕ К СВОЕМУ ТИПУ, А НЕ К ЧУЖОМУ.
            //
            // Здесь стояло приведение к webrtc::RtpFrameObject. Оно было верным лишь потому,
            // что путь до сюда шёл мимо буфера показа: при склейке многослойной единицы
            // контроллер отдаёт CombineAndDeleteFrames, то есть плоский EncodedFrame, и
            // приведение стало бы неопределённым поведением. Наш тип создаём мы сами, и он
            // единственный, который сюда доходит.
            auto* rtp_frame = static_cast<OmniAssembledFrame*>(frame.get());
            webrtc::RTPVideoHeader video_header = rtp_frame->GetRtpVideoHeader();
            std::vector<uint8_t> annex_b;
            bool rebuilt = false;
            {
                std::lock_guard<std::mutex> lock(mutex_);
                rebuilt = BuildH264AnnexBFrameFromCacheUnsafe(
                    rtp_frame->first_seq_num(),
                    rtp_frame->last_seq_num(),
                    annex_b);
                if (rebuilt) {
                    ++h264_annexb_rebuilds_;
                } else {
                    ++h264_annexb_rebuild_failures_;
                }
            }

            if (rebuilt) {
                if (AnnexBContainsH264NaluType(annex_b, static_cast<uint8_t>(webrtc::H264::NaluType::kIdr))) {
                    ++h264_receiver_idr_frames_;
                }
                if (AnnexBContainsH264NaluType(annex_b, static_cast<uint8_t>(webrtc::H264::NaluType::kSps))) {
                    ++h264_receiver_sps_frames_;
                }
                if (AnnexBContainsH264NaluType(annex_b, static_cast<uint8_t>(webrtc::H264::NaluType::kPps))) {
                    ++h264_receiver_pps_frames_;
                }
                image.SetEncodedData(webrtc::EncodedImageBuffer::Create(
                    annex_b.data(),
                    annex_b.size()));
            } else {
                webrtc::video_coding::H264SpsPpsTracker::FixedBitstream fixed =
                    h264_sps_pps_tracker_.CopyAndFixBitstream(
                        std::span<const uint8_t>(frame->data(), frame->size()),
                        &video_header);
                if (fixed.action != webrtc::video_coding::H264SpsPpsTracker::kInsert ||
                    fixed.bitstream.empty()) {
                    ++decode_failures_;
                    if (fixed.action == webrtc::video_coding::H264SpsPpsTracker::kRequestKeyframe) {
                        ++h264_sps_pps_tracker_keyframe_requests_;
                    } else {
                        ++h264_sps_pps_tracker_drops_;
                    }
                    {
                        std::lock_guard<std::mutex> lock(mutex_);
                        RequestReceiverKeyframeUnsafe(MonotonicNowMs(), kReasonDecodeFailure);
                    }
                    return false;
                }

                image.SetEncodedData(webrtc::EncodedImageBuffer::Create(
                    fixed.bitstream.data(),
                    fixed.bitstream.size()));
            }
        }

#if defined(_WIN32)
        // Task #84 increment 3: HW H.264/HEVC decode (DXVA → NV12 texture): feed it the Annex-B and
        // present the NV12 texture. produced ⇒ render; buffered (decoder latency) ⇒ wait, the HW
        // decoder owns this frame; hard failure / store failure ⇒ drop the HW decoder, ask for a
        // key frame, and the next frame brings up a fresh one.
        // Neither has a software decoder, so they cannot wait for the GPU render path to attach (it
        // never does while the call window is hidden — live 2026-10-13, the laptop in the tray):
        // the Media Foundation decoder comes up on the renderer's device on the first frame.
        if (!HasSoftwareVideoCodec(codec_kind_) && !hardware_h264_decoder_ &&
            !force_software_decode_ && video_renderer_.Initialize() &&
            video_renderer_.device() != nullptr) {
            std::string decoder_error;
            hardware_h264_decoder_ = MediaFoundationH264Decoder::Create(
                video_renderer_.device(), width_, height_, decoder_error, codec_kind_);
        }
        // AV1 decodes in software (dav1d) everywhere and on the GPU where the Media Foundation AV1
        // decoder opens: tried once, on the stream's first frame; a decoder that fails hands the
        // stream back to dav1d for good (it asks for a key frame on its first refusal).
        if (codec_kind_ == NativeVideoCodec::Av1 && !hardware_av1_tried_ && frames_fed_to_decoder_ == 1 &&
            !force_software_decode_ && video_renderer_.Initialize() &&
            video_renderer_.device() != nullptr) {
            hardware_av1_tried_ = true;
            std::string decoder_error;
            hardware_h264_decoder_ = MediaFoundationH264Decoder::Create(
                video_renderer_.device(), width_, height_, decoder_error, codec_kind_);
        }
        if (hardware_h264_decoder_ &&
            image.data() != nullptr && image.size() > 0) {
            Microsoft::WRL::ComPtr<ID3D11Texture2D> nv12;
            UINT subresource = 0;
            bool produced = false;
            const OmniWebRtcMediaStatus hw_status = hardware_h264_decoder_->Decode(
                image.data(), image.size(),
                static_cast<int64_t>(MonotonicNowMs()) * 10'000, nv12, subresource, produced);
            if (hw_status == OMNI_WEBRTC_MEDIA_STATUS_OK) {
                if (!produced || !nv12) {
                    // Кадр принят декодером и выйдет позже — это УСПЕХ, а не потеря, и
                    // требовать из-за него ключевой кадр было бы неверно.
                    return true;  // buffered — emits on a later feed; do NOT SW-decode
                }
                // No swapchain: the picture goes to the software presentation queue instead.
                if (!video_renderer_.HasSwapChain()) {
                    if (DeliverHwNv12AsI420(nv12.Get(), subresource, image.RtpTimestamp())) {
                        ++frames_decoded_;
                        ++hw_frames_decoded_;
                        return true;
                    }
                } else if (StoreHwNv12Frame(nv12.Get(), subresource)) {
                    ++frames_decoded_;
                    ++hw_frames_decoded_;
                    return true;
                }
            }
            // Hard HW failure or store failure: drop this decoder. H.264/HEVC lose the frame (no
            // software decoder) and the next frame brings up a fresh one; AV1 falls to dav1d below.
            hardware_h264_decoder_.reset();
            ++hw_decode_failures_;
        }
#endif

        // H.264 and HEVC have only the hardware decoder above; without it nothing decodes them.
        const int decode_status = decoder_ ? decoder_->Decode(image, 0) : WEBRTC_VIDEO_CODEC_ERROR;
        if (decode_status == WEBRTC_VIDEO_CODEC_OK) {
            (void)decode_callback_.WaitForFrame(std::chrono::milliseconds(1000));
            ++frames_decoded_;
            return true;
        }
        ++decode_failures_;
        ++h264_decoder_status_failures_;
        last_decode_status_ = decode_status;
        {
            // What the decoder refused: size, key flag and the first bytes — enough to tell a
            // bitstream without its sequence header from a truncated or foreign frame.
            char head[96];
            int written = std::snprintf(head, sizeof(head), "%zuB%s:", image.size(),
                                        image._frameType == webrtc::VideoFrameType::kVideoFrameKey ? "K" : "d");
            for (size_t b = 0; b < std::min<size_t>(image.size(), 12) && written > 0 && written < 90; ++b) {
                written += std::snprintf(head + written, sizeof(head) - written, "%02x", image.data()[b]);
            }
            std::lock_guard<std::mutex> lock(mutex_);
            last_failed_frame_head_ = head;
        }
        {
            std::lock_guard<std::mutex> lock(mutex_);
            RequestReceiverKeyframeUnsafe(MonotonicNowMs(), kReasonDecodeFailure);
        }
        return false;
    }

    // Объявлен ДО env_: окружение держит журнал по указателю, значит журнал обязан родиться раньше
    // и умереть позже всего, что это окружение получило.
    OmniBweEventLog bwe_event_log_;
    webrtc::Environment env_ = MakeOmniEnvironment(&bwe_event_log_);
    std::unique_ptr<webrtc::VideoEncoderFactory> encoder_factory_;
    std::unique_ptr<webrtc::VideoDecoderFactory> decoder_factory_;
    std::unique_ptr<webrtc::VideoDecoder> decoder_;
#if defined(_WIN32)
    /// Не владеет: это тот же объект, что и encoder_, когда выбран аппаратный путь.
    /// Нужен ради диагностики MFT, которой у интерфейса webrtc::VideoEncoder нет.
    /// Фабрика, у которой оркестратор просит кодировщик. Не владеет — владеет encoder_factory_.
    /// Почему кодируем процессором, а не GPU. Пусто = аппаратный кодировщик поднялся.
    /// Видно в диагностике звонка, иначе откат был бы молчаливым.
    std::string hardware_encoder_fallback_reason_;
#endif
    SessionEncodeCallback encode_callback_;
    SessionDecodeCallback decode_callback_;
#if defined(_WIN32)
    omni_media::D3D11VideoRenderer video_renderer_;
    // Task #84 increment 3: HW H.264/HEVC decode on the renderer's device → NV12 texture → rendered
    // zero-copy. Created on the first frame or in AttachVideoRenderer; the only decoder for these
    // two codecs (AV1 decodes in software through `decoder_`). hw_nv12_render_tex_ is a stable,
    // SRV-capable copy of the latest HW-decoded frame (the MFT output may be a reused array slice
    // and/or lack SHADER_RESOURCE). has_hw_nv12_frame_ gates the NV12 render in RenderAttachedVideo.
    std::unique_ptr<MediaFoundationH264Decoder> hardware_h264_decoder_;
    Microsoft::WRL::ComPtr<ID3D11Texture2D> hw_nv12_render_tex_;
    int hw_nv12_width_ = 0;
    int hw_nv12_height_ = 0;
    bool has_hw_nv12_frame_ = false;
    uint64_t hw_frames_decoded_ = 0;
    // Frames handed to a decoder so far: a hardware decoder created after the first one joins a
    // stream already under way and must start from a key frame (AttachVideoRenderer).
    uint64_t frames_fed_to_decoder_ = 0;
    uint64_t hw_decode_failures_ = 0;
    /// AV1 tries the Media Foundation decoder once per receiver (dav1d is behind it).
    bool hardware_av1_tried_ = false;
#endif
    NativeVideoCodec codec_kind_ = NativeVideoCodec::H264;
    std::unique_ptr<webrtc::RtpVideoFrameAssembler> assembler_;
    webrtc::video_coding::H264SpsPpsTracker h264_sps_pps_tracker_;
    int width_ = 640;
    int height_ = 360;
    int fps_ = 30;
    int bitrate_bps_ = 800'000;
    // Task #87 device selection (from session config; 0 = Auto). encode vendor -> encoder MFT
    // filter; decode vendor -> renderer adapter (the HW decoder binds to it); decode-software
    // skips the HW decoder. There is no encode-software member: H.264 encode is HW-only for now.
    uint32_t preferred_encode_vendor_id_ = 0;
    int test_hw_encode_fail_after_ = 0;
    /// Согласованный предел одного RTP-пакета, байт. См. kDefaultMaxRtpPacketBytes.
    size_t max_rtp_packet_bytes_ = kDefaultMaxRtpPacketBytes;
    uint32_t preferred_decode_vendor_id_ = 0;
    bool force_software_decode_ = false;
    /// Кодировать процессором даже если аппаратный кодировщик доступен. Симметрично
    /// `force_software_decode_`; нужен и для диагностики, и как выбор пользователя.
    bool force_software_encode_ = false;
    /// Демонстрация экрана: другой размен под нехваткой полосы — см. разбор конфигурации.
    bool screen_content_ = false;
    int negotiated_width_ = 640;
    int negotiated_height_ = 360;
    int negotiated_fps_ = 30;
    int negotiated_bitrate_bps_ = 800'000;
    int abr_target_width_ = 640;
    int abr_target_height_ = 360;
    int abr_target_fps_ = 30;
    int abr_target_bitrate_bps_ = 800'000;
    // С какими числами кодировщик УЖЕ настроен. Повторять их незачем: каждая перенастройка —
    // полный ReconfigureEncoder на очереди кодирования (см. UpdateAbr).
    int last_configured_bitrate_bps_ = 0;
    int last_configured_fps_ = 0;
    uint64_t abr_resolution_changes_ = 0;
    // Wave 7 (2026-08-17): расхождение между тем, что мы у кодировщика ПРОСИМ, и тем, что он
    // отдаёт. Замерено в этот день: цель 150 кбит/с -> аппаратный H.264 кладёт 605, программный 331
    // (docs/decisions/video-quality-ceilings-and-twcc.md). Раньше на выход не смотрел никто, и вся
    // адаптация работала вслепую: считала свою просьбу исполненной.
    //
    // Узел апстримовский, а не самописный: AdjustRateAllocation ведёт трёхсекундное окно
    // фактического выхода и правит число, которое мы отдаём кодировщику, чтобы в цель попал ФАКТ.
    // Нужен прежде всего АППАРАТНОМУ пути: у MFT нет флага пропуска кадров, поэтому единственный
    // способ добиться от него нужной скорости — просить меньше, чем хочешь. Программному путь
    // выправляет собственный пропуск кадров (см. SetFrameDropEnabled в BuildSmokeCodec).
    //
    // Живёт между ДВУМЯ потоками: наполняется на потоке кодирования (OnEncodedFrame), а
    // спрашивается с потока ABR (AdjustRateAllocation). Свой мьютекс, а не общий mutex_: общий
    // держится на путях, где кодирование и так сериализовано, и вешать на него ещё и обратную связь
    // значит связать два несвязанных темпа.
    // ОЦЕНКА КАНАЛА ПО ФАКТИЧЕСКОЙ ДОСТАВКЕ (RFC 8888).
    //
    // До этого GoogCC — сердце всей адаптации — работал на потерях и RTT, причём RTT в диагностике
    // всегда стоял нулём. То есть регулятор реагировал ПОСТФАКТУМ, по уже случившимся потерям,
    // вместо того чтобы видеть нарастающую задержку заранее. Всё, что мы чинили в исполнении его
    // решений, стояло на неполных входных данных.
    //
    // Обе половины апстримовские: приёмник копит времена прибытия и выдаёт RTCP-отчёт
    // (CongestionControlFeedbackGenerator), отправитель сопоставляет отчёт со своей историей
    // отправки и отдаёт GoogCC (TransportFeedbackAdapter). Отдельного расширения заголовка не
    // нужно: RFC 8888 ключуется по SSRC и номеру RTP-пакета, которые в пакете и так есть.
    /// Времена отправки по транспортному номеру пакета — ТОЛЬКО ради RTT.
    /// См. NotifyModulePacketSent: всё остальное о доставке считает сборщик.
    std::map<int64_t, webrtc::Timestamp> sent_at_by_packet_id_;
    // Разворачивает номера медиапотока для ключа выше; меняется только под feedback_mutex_.
    webrtc::RtpSequenceNumberUnwrapper sent_sequence_unwrapper_;
    static constexpr size_t kMaxSentAtEntries = 2048;
    std::unique_ptr<webrtc::CongestionControlFeedbackGenerator> feedback_generator_;
    std::mutex feedback_mutex_;
    // Построено генератором — САМЫЙ ВЕРХНИЙ счётчик этой цепочки. Атомарный, потому что пишется
    // под feedback_mutex_, а читается диагностикой под mutex_.
    std::atomic<uint64_t> transport_feedback_reports_built_{0};
    uint64_t transport_feedback_reports_applied_ = 0;
    uint64_t transport_feedback_packets_reported_ = 0;
    // Последняя оценка канала, ПОСТРОЕННАЯ НА ДОСТАВКЕ, а не на потерях. Отдельным полем, чтобы в
    // диагностике было видно, расходится ли она с тем, что регулятор насчитал по старым входам.
    int goog_cc_feedback_target_bps_ = 0;
    // Измеренный проход туда-обратно, в МИКРОсекундах и с -1 как «ещё не измеряли».
    //
    // В миллисекундах не годится: на петле обратный путь короче миллисекунды и округлялся в
    // ноль, а ноль в этом файле уже означает «нет данных». Получалось, что «измерили 0.4 мс» и
    // «не измеряли вовсе» читаются одинаково — ровно та подмена, из-за которой RTT и простоял
    // ненайденным.
    int64_t transport_rtt_us_ = -1;
    // Сколько кластеров проб мы создали. Ноль означает, что пробинга нет вовсе — а именно так
    // и было до 2026-08-18, при живом ProbeController, который их исправно запрашивал.
    // #50 этап 1 шаг 2: модуль создан, но провод на него ещё не переключён. Эти два
    // счётчика — единственный способ увидеть, что он жив, и число для следующего шага.
    // Факт создания — ОТДЕЛЬНЫМ флагом, а не выводом из счётчиков.
    //
    // Сначала я собирался судить о жизни модуля по счётчикам его RTP и RTCP.
    // Это негодная проверка: пока провод не переключён и RTCP выключен, оба счётчика равны
    // нулю — то есть «модуль создан и молчит» неотличимо от «модуль не создался». Критерий,
    // который не может быть опровергнут, ничего и не подтверждает.
    std::atomic<bool> module_created_{false};
    std::atomic<uint64_t> module_rtp_sent_{0};
    // Кадры, которые отправитель не взял (модуль не поднялся).
    std::atomic<uint64_t> module_send_failures_{0};
    // Что ушло на провод помимо свежего видео. Обе величины снимаются в точке отправки, а не там,
    // где их запросили: «попросили повторить» и «повтор ушёл» — разные числа, и вторым надо
    // проверять первое. module_padding_sent_ пока обязан быть нулём — добивка не включена.
    std::atomic<uint64_t> module_retransmissions_sent_{0};
    std::atomic<uint64_t> module_padding_sent_{0};
    // RTCP на проводе: сколько выпустили и сколько приняли. Обе величины отличают «канал обратной
    // связи живой» от «мы его построили и он молчит» — различие, которое за эту сессию уже дважды
    // оказывалось решающим.
    std::atomic<bool> receiver_rtcp_ready_{false};
    std::atomic<bool> receiver_rtcp_enabled_{false};
    // Факт создания сборщика — ОТДЕЛЬНЫМ флагом, а не выводом из счётчиков. Пока через него
    // ничего не идёт, любой счётчик отправки будет нулём при обоих исходах, и «создан» станет
    // неотличимо от «не создан». Тот же урок, что и с module_created_.
    std::atomic<bool> transport_controller_created_{false};
    /// ОРКЕСТРАТОР КОДИРОВАНИЯ (#61, шаг D2b) и всё, что он держит на входе и выходе.
    ///
    /// Источник и приёмник переживают его намеренно: он держит на них сырые указатели.
    std::unique_ptr<OmniEncoderStatsObserver> encoder_stats_observer_;
    std::unique_ptr<OmniPushVideoSource> push_video_source_;
    std::unique_ptr<OmniEncoderSink> encoder_sink_;
    std::unique_ptr<webrtc::VideoBitrateAllocatorFactory> bitrate_allocator_factory_;
    std::unique_ptr<webrtc::VideoStreamEncoder> video_stream_encoder_;
    /// PLI собеседника -> оркестратор (апстримовский переходник; см. CreateRtpVideoSender).
    std::unique_ptr<webrtc::EncoderRtcpFeedback> encoder_rtcp_feedback_;
    /// Что приёмный модуль отправил на провод и что принял отправитель видео (NACK, PLI).
    /// Члены, а не кучевые объекты: модули держат на них сырые указатели и умирают раньше сессии.
    OmniRtcpTypeCounter receiver_rtcp_sent_{kOmniVideoSsrc};
    OmniRtcpTypeCounter sender_rtcp_received_{kOmniVideoSsrc};
    OmniReportBlockObserver sender_report_blocks_{kOmniVideoSsrc};
    /// Давность обратной связи на часах такта ABR: сколько отчётов (RR + о доставке) видел прошлый
    /// такт, когда пришёл последний новый и когда мы начали слать. См. kAbrFeedbackLostMs.
    uint64_t abr_feedback_seen_ = 0;
    int64_t abr_last_feedback_ms_ = 0;
    int64_t abr_sending_since_ms_ = 0;
    /// RTT приёмного модуля из XR; последнее значение — для диагностики (-1: ещё не измерен).
    std::atomic<int64_t> receiver_rtt_ms_{-1};
    OmniRttObserver receiver_rtt_observer_{[this](int64_t rtt_ms) {
        receiver_rtt_ms_.store(rtt_ms, std::memory_order_relaxed);
        ApplyReceiverRtt(rtt_ms);
    }};
    uint64_t encoder_dropped_frames_ = 0;
    uint64_t encoder_target_updates_ = 0;
    int64_t encoder_last_target_bps_ = -1;
    /// Подписан ли источник кадров оркестратором. См. EncodeVideoFrameBuffer.
    std::atomic<bool> encoder_source_ready_{false};
    /// Начальная метка времени RTP, выбранная модулем. См. CreateRtpVideoSender.
    std::atomic<uint32_t> rtp_start_timestamp_{0};
    // RTCP: отдано в ядро (сразу, без очереди) и принято.
    std::atomic<uint64_t> rtcp_packets_sent_{0};
    std::atomic<uint64_t> rtcp_packets_received_{0};
    // Куда уходят исходящие пакеты сессии. См. OmniPacketSink.
    OmniPacketSink packet_sink_;
    // Разрешено ли пробовать без крупного медиапакета. Сейчас ВСЕГДА ноль: строка, которая это
    // включала, откачена по замеру (см. комментарий в создании модуля). Поле оставлено намеренно —
    // оно показывает в диагностике, что режим выключен, и не даст включить его молча.
    std::atomic<bool> probe_without_media_allowed_{false};
    // Сказал ли GoogCC хоть раз своё слово о пейсере. Пока не сказал — держим свою
    // стартовую настройку; как только сказал, настройка принадлежит ему, и затирать её
    // нельзя: иначе pacer_config от регулятора живёт лишь до ближайшего такта ABR.
    // ПОСЛЕДНЯЯ известная оценка GoogCC, а не «оценка, пришедшая в этот раз».
    //
    // Регулятор отдаёт цель только при её ИЗМЕНЕНИИ. Пока мы возвращали то, что пришло на
    // данном вызове, устоявшаяся — то есть верная — оценка читалась как «мнения нет», и вместо
    // неё работала лестница восстановления: потерь нет, RTT ноль, очередь пуста, значит
    // «стабильно», значит можно разгоняться. На стенде с затором это выглядело так: оценка по
    // доставке 586818 бит/с при канале 600000, а цель ползла обратно к запрошенным 1.5 Мбит/с
    // (2026-08-18).
    //
    // Та же ошибка, что и с потерянной обратной связью, только зеркальная: «нового не сказал»
    // — это не «мнения нет», а «мнение прежнее».
    int goog_cc_last_target_bps_ = 0;
    // Когда её запомнили. Память со сроком годности, а не навсегда: оценка осмысленна ровно
    // пока её подпитывают. Без срока первая же просадка держала бы кадр внизу вечно —
    // проверено на лестнице разрешений, где трафика нет вовсе и поднять оценку нечем.
    // Часы здесь те же, что у такта ABR (sample.now_ms), а не стенные: у стендов они свои.
    int64_t goog_cc_last_target_at_ms_ = 0;
    // Какую оценку по доставке уже учли. Нужна, чтобы отличить «пришла новая» от «лежит
    // старая»: отчёты приходят чаще тактов ABR, и без этого свежесть было бы не с чем сверить.
    int goog_cc_feedback_target_consumed_ = 0;
    double abr_loss_ewma_q8_ = 0.0;
    double abr_rtt_ewma_ms_ = 0.0;
    double abr_jitter_ewma_ = 0.0;
    uint32_t abr_state_ = OMNI_WEBRTC_MEDIA_ABR_STATE_STABLE;
    std::string abr_reason_ = "stable";
    // Wave 4 Phase 2a (2026-05-16): GoogCC NetworkController is constructed
    // alongside the legacy state machine but NOT YET consulted by UpdateAbr.
    // Phase 2b will switch UpdateAbr over once build linkage is verified
    // (CMakeLists obj files might need iteration depending on which sub-
    // libraries the linker pulls). See docs/decisions/googcc-adoption.md.
    /// Сколько пакетов было отдано на момент прошлого такта GoogCC. Дельта от него — реальное
    /// число пакетов за интервал; прежде оно выдумывалось из «~300 в секунду».
    // Наблюдатель загрузки, который мы отдаём детектору перегрузки внутри оркестратора
    // кодирования. Своих детекторов у сессии больше нет (#61, шаг D3): и перегрузка процессора,
    // и рост QP измеряются внутри VideoStreamEncoder, потому что мерить надо ТОТ кодировщик,
    // который работает, а он принадлежит оркестратору.
    /// Вход сканера полосы, записанный там, где он проходит (см. OnEncodedImageReady).
    struct EncodedSample
    {
        int64_t at_ms = 0;
        size_t bytes = 0;
    };
    mutable std::mutex encoded_window_mutex_;
    mutable std::deque<EncodedSample> encoded_window_;
    mutable size_t encoded_window_bytes_ = 0;
    std::atomic<int> encoded_width_{0};
    // QP of the last encoded frame as the quality scaler sees it (-1: none parsed).
    std::atomic<int> encoded_qp_{-1};
    std::atomic<int> encoded_height_{0};

    /// Вход и выход аппаратного кодировщика, считанные внутри него. shared_ptr, потому что
    /// кодировщиком владеет оркестратор и пересоздаёт его — см. комментарий у OmniEncoderIoStats.
    std::shared_ptr<OmniEncoderIoStats> encoder_io_stats_;

    std::unique_ptr<NullCpuOveruseMetricsObserver> cpu_overuse_metrics_observer_;
    std::unique_ptr<webrtc::TaskQueueBase, webrtc::TaskQueueDeleter> adaptation_task_queue_;
    // #50 этап 1: у модуля своя очередь, а не общая с адаптацией. Он берёт её прямо в
    // конструкторе (worker_queue_(TaskQueueBase::Current()), rtp_rtcp_impl2.cc:84), держит три
    // sequence-checker'а и секундный таймер RTCP. Отдельная очередь даёт понятное владение и
    // понятный порядок разрушения — ровно то, чего не хватало деструктору до 2026-08-23.
    std::unique_ptr<webrtc::TaskQueueBase, webrtc::TaskQueueDeleter> rtp_task_queue_;
    std::unique_ptr<OmniModuleTransport> module_transport_;
    // Приёмная сторона: настоящий RTCP вместо структурных событий (#57, шаг A).
    std::unique_ptr<OmniRtcpTransport> rtcp_transport_;
    std::unique_ptr<webrtc::ReceiveStatistics> receive_statistics_;
    std::unique_ptr<webrtc::ModuleRtpRtcpImpl2> receiver_rtcp_module_;
    // PacketRouter САМ является PacingController::PacketSender, а TaskQueuePacedSender — сразу
    // и RtpPacketPacer, и RtpPacketSender. Поэтому свой адаптер и свой ручной прогон пейсера не
    // переписывались, а были удалены целиком: у апстрима эти роли уже заняты. Оба объекта берут
    // очередь из TaskQueueBase::Current(), значит строятся в той же задаче, что и модуль.
    // #57 шаг C1: сборщик. Пока построен и неподвижен — см. комментарий при создании.
    std::unique_ptr<OmniTargetRateObserver> target_rate_observer_;
    std::unique_ptr<webrtc::RtpTransportControllerSend> transport_controller_;
    // Владеет им сборщик, а не мы: создаётся через CreateRtpVideoSender и снимается
    // DestroyRtpVideoSender. Поэтому сырой указатель, а не unique_ptr.
    webrtc::RtpVideoSenderInterface* rtp_video_sender_ = nullptr;
    // Пакетизация, расширения заголовка и нарезка под лимит пакета. Живёт и умирает на
    // rtp_task_queue_: SendVideo помечен RTC_CHECK_RUNS_SERIALIZED, то есть звать его можно
    // только с одной последовательности. Держит сырой RTPSender* модуля — значит обязан
    // умереть РАНЬШЕ модуля.
    // Шаги адаптации оркестратора, учтённые прошлым тактом ABR. Прирост — это и есть событие:
    // счётчики монотонные, и без снимка одно старое событие срабатывало бы каждый такт.
    int abr_seen_cpu_steps_ = 0;
    int abr_seen_quality_steps_ = 0;
    // Wave 5b (2026-05-18): NackRequester + helpers. Lifetimes:
    //   nack_periodic_processor_ is a standalone object — owns its
    //     repeating ProcessNackModules task which lives on the task queue
    //     that called RegisterNackModule. Must outlive nack_requester_.
    //   nack_requester_ MUST be constructed on adaptation_task_queue_
    //     (its sequence checker binds in the constructor) and MUST be
    //     destroyed on the same queue (otherwise NackPeriodicProcessor
    //     unregistration races with the periodic task).
    //   receiver_rtcp_feedback_ only posts to the rtp queue; it must outlive
    //     nack_requester_, which holds raw pointers to it.
    std::unique_ptr<webrtc::NackPeriodicProcessor> nack_periodic_processor_;
    std::unique_ptr<webrtc::NackRequester> nack_requester_;
    std::unique_ptr<OmniReceiverRtcpFeedback> receiver_rtcp_feedback_;
    // ПОКАЗ ПРИНАДЛЕЖИТ АПСТРИМУ ЦЕЛИКОМ (#60).
    //
    // Здесь лежали FrameBuffer + JitterEstimator, а решение «пора ли декодировать» принимала наша
    // DecodeDuePlayoutFrames по возрасту кадра. Это была ровно та же ошибка, что и на передающей
    // половине: листья взяты у апстрима, оркестратор написан свой — и все три дефекта #56 жили
    // именно в оркестраторе, ни один в листьях.
    //
    // Теперь связку держит VideoStreamBufferController: он владеет своим FrameBuffer,
    // JitterEstimator, расчётом межкадрового разброса, планировщиком показа и сторожем
    // декодируемости. Мы отдаём ему собранные кадры и забираем те, которые он назначил к показу.
    //
    // РАЗДЕЛЕНИЕ ПОТОКОВ — АПСТРИМОВСКОЕ, И ЭТО НЕ ФОРМАЛЬНОСТЬ.
    //   playout_task_queue_ = роль worker_thread у апстрима: только учёт, только контроллер.
    //   поток оболочки, зовущий Drain*/Render* = роль decode_queue_: декодирование и ВСЯ работа с
    //     D3D11.
    // Второе — не стиль, а необходимость: аппаратный декодер создан на устройстве рендерера
    // (video_renderer_.device()), и контекст устройства не потокобезопасен. Утащи декодирование на
    // очередь показа — и декодер с рендерером начнут работать с одним контекстом с двух потоков.
    std::unique_ptr<webrtc::TaskQueueBase, webrtc::TaskQueueDeleter> playout_task_queue_;
    std::unique_ptr<webrtc::VCMTiming> playout_timing_;
    std::unique_ptr<OmniPlayoutStatsObserver> playout_stats_;
    std::unique_ptr<OmniPlayoutReceiver> playout_receiver_;
    std::unique_ptr<webrtc::VideoStreamBufferController> buffer_controller_;
    /// Кадр, назначенный к показу и ждущий потока-декодировщика. Глубина 1 по построению:
    /// контроллер не отдаёт следующий, пока не позовут StartNextDecode.
    std::unique_ptr<webrtc::EncodedFrame> pending_decode_frame_;
    std::mutex pending_decode_mutex_;
    /// Трогается только на очереди показа (постановка) и в потоке-декодировщике (чтение под
    /// pending_decode_mutex_ не нужно: это простой флаг «контроллер запущен»).
    std::atomic<bool> playout_started_{false};
    /// true, пока не декодирован ни один кадр или пока декодирование не удалось. Смысл тот же,
    /// что у keyframe_required_ в VideoReceiveStream2.
    std::atomic<bool> playout_keyframe_required_{true};
    std::atomic<uint64_t> decode_timeouts_{0};
    /// Сколько кадров лежит в буфере показа. Обновляется на очереди показа.
    std::atomic<uint64_t> playout_pending_frames_{0};
    /// Кадры, отброшенные потому, что ждём ключевой: опоры для них нет.
    std::atomic<uint64_t> playout_frames_awaiting_keyframe_{0};
    /// Метки времени кадров, в которых участвовал восстановленный из RTX пакет. Нужны, чтобы
    /// честно ответить буферу показа на вопрос delayed_by_retransmission(). См. OmniAssembledFrame.
    std::set<uint32_t> retransmitted_frame_timestamps_;
    std::deque<std::string> abr_reason_history_;
    // Atomic counters: see docs/decisions/native-media-locking.md.
    // These are updated from caller-thread Push/Drain paths and read from
    // the same paths plus diagnostics readers. Without atomicity, concurrent
    // push would cause C++ data-race UB even on strong-memory-order x86.
    const uint32_t ssrc_ = kOmniVideoSsrc;
    std::atomic<uint64_t> synthetic_frame_index_{0};

    mutable std::mutex mutex_;
    // Wave 5 Phase 5a-step2: pace_budget_bytes_ / pace_last_refill_ms_
    // removed (PacingController owns pacing state).
    // СВОЙ КЭШ ОТПРАВЛЕННОГО УБРАН (#53). Его писали на каждом выданном пакете и не читали
    // больше нигде: повтор теперь достаёт пакет из RtpPacketHistory внутри модуля, и делает это
    // точнее — хранит пакеты, а не байты, и знает время последней отправки каждого. Оставлять
    // копию каждого пакета «на всякий случай» значило бы платить памятью и временем за структуру,
    // у которой нет ни одного читателя.
    std::map<uint16_t, std::vector<uint8_t>> received_rtp_payload_cache_;
    std::deque<uint16_t> received_rtp_payload_order_;
    size_t received_rtp_payload_cache_bytes_ = 0;
    // Wave 5c (2026-05-18): playout_frames_ deque + playout_bytes_ counter
    // removed. FrameBuffer owns the inflight frames (frame_buffer_->CurrentSize());
    // total bytes are no longer tracked because FrameBuffer caps by frame
    // count, not by byte budget.

    // The frames_encoded_ counter is read/written from the caller's
    // push-encode thread (outside the mutex_ critical section). Marked
    // atomic to eliminate the data race UB the audit flagged.
    // Other counters here are updated only under mutex_ (per-call code
    // review confirms it), so they stay as plain uint64_t for now.
    std::atomic<uint64_t> frames_encoded_{0};
    uint64_t frames_decoded_ = 0;
    uint64_t frames_drained_ = 0;
    // Отдано в ядро — для отчёта отправителя. Атомарные: их пишет поток пейсера.
    std::atomic<uint64_t> rtp_packets_sent_{0};
    std::atomic<uint64_t> rtp_octets_sent_{0};
    uint64_t rtp_packets_received_ = 0;
    // Кадры по признаку «ссылаются ли на них». Доля непривязанных = реально работающая структура
    // временных слоёв, а не строчка в конфиге.
    // Сквозные, 64-битные и БЕЗ переполнения. На провод RTPSenderVideo кладёт младшие 16 бит
    // сам (frame_id & 0xFFFF), а разность frame_id - dependency считает в int64 — поэтому
    // счётчик обязан быть широким, иначе на 65536-м кадре разность станет отрицательной.
    // Приёмная сторона: описатели структуры кадров, ПРИШЕДШИЕ ПО ПРОВОДУ. См. PushRtpPacket.
    uint64_t received_generic_descriptors_ = 0;
    uint64_t received_generic_dependencies_ = 0;
    // Поток RTX на приёме. Разбор — апстримовый, наш только адаптер приёмника.
    //
    // Порядок разрушения: поток ссылается на адаптер, адаптер на сессию. Значит поток умирает
    // первым; оба объявлены здесь и уничтожаются в обратном порядке объявления.
    std::unique_ptr<OmniRtxMediaSink> rtx_media_sink_;
    std::unique_ptr<webrtc::RtxReceiveStream> rtx_receive_stream_;
    uint64_t rtx_packets_received_ = 0;
    uint64_t rtx_packets_recovered_ = 0;
    uint64_t frames_reference_ = 0;
    uint64_t frames_non_reference_ = 0;
    /// Выход СБОРЩИКА кадров. Что с кадром сделал буфер показа — спрашивается у контроллера.
    uint64_t rtp_frames_assembled_ = 0;
    // Диагностика обвала сборки: сколько раз сработал сторож «пакеты идут, кадры не собираются» и
    // какой была самая длинная пауза между собранными кадрами. Вместе они отличают «сборка встала
    // насовсем» от «сборка идёт редко» — а это разные болезни.
    uint64_t no_assembled_watchdog_fired_ = 0;
    int64_t max_assembly_gap_ms_ = 0;
    // Отказы буфера показа, РАЗДЕЛЁННЫЕ по причине. Пока они были неразличимы, потеря кадров по
    // ёмкости выглядела в диагностике ровно как безобидная избыточность.
    uint64_t rate_updates_ = 0;
    uint64_t keyframe_requests_ = 0;
    uint64_t encode_failures_ = 0;
    // Кадры, которые кодировщик СОЗНАТЕЛЬНО не закодировал, удерживая битрейт. Заменило
    // encode_timeouts_: пока пропуск был выключен, пустой результат действительно означал зависание,
    // теперь он означает работающее регулирование, и считать его «таймаутом» значит врать в
    // диагностике ровно про тот механизм, которым мы теперь и держим канал.
    uint64_t frames_skipped_by_encoder_ = 0;
    uint64_t decode_failures_ = 0;
    uint64_t h264_decoder_status_failures_ = 0;
    uint64_t h264_sps_pps_tracker_drops_ = 0;
    uint64_t h264_sps_pps_tracker_keyframe_requests_ = 0;
    uint64_t h264_annexb_rebuilds_ = 0;
    uint64_t h264_annexb_rebuild_failures_ = 0;
    uint64_t h264_receiver_idr_frames_ = 0;
    uint64_t h264_receiver_sps_frames_ = 0;
    uint64_t h264_receiver_pps_frames_ = 0;
    // Пакеты, пришедшие не по порядку (без восстановленных). Диагностика, не потеря.
    uint64_t receiver_out_of_order_packets_ = 0;
    std::atomic<int> presented_width_{0};
    std::atomic<int> presented_height_{0};
    // Wave 5b (2026-05-18): receiver_missing_packets_dropped_at_cap_ removed
    // along with the self-written missing-packet tracker. NackRequester
    // bounds its own state internally (kMaxNackPackets=1000) and escalates
    // to KeyFrameRequestSender when it can't keep up.
    // Wave 5b (2026-05-18): last_nack_ms_ removed — NackRequester runs
    // its own ProcessNacks tick every 20ms and computes per-packet
    // send-at times internally (NackInfo::send_at_seq_num).
    int64_t last_keyframe_request_ms_ = 0;
    // When a frame last came OUT of the assembler (0 = the stream has not started). Guards the
    // receiver's blind spot: packets in, frames out = 0. See kReasonNoAssembledFrames.
    int64_t last_assembled_frame_ms_ = 0;
    uint32_t receiver_ssrc_ = 0;
    uint16_t receiver_highest_sequence_ = 0;
    uint32_t receiver_sequence_cycles_ = 0;
    uint32_t last_keyframe_request_reason_ = 0;
    int last_decode_status_ = 0;
    std::vector<uint8_t> h265_vps_;
    std::vector<uint8_t> h265_sps_;
    std::vector<uint8_t> h265_pps_;
    uint64_t h265_parameter_set_prepends_ = 0;
    std::string last_failed_frame_head_;
    // Wave 5b (2026-05-18): receiver_missing_packets_ map removed —
    // NackRequester owns the missing-packet tracking internally.
    bool has_receiver_sequence_ = false;
    bool force_next_keyframe_ = false;
};

// Определены вне класса, чтобы видеть полный тип LibWebRtcVideoSession — он объявлен
// предварительно выше транспорта.
bool OmniRtcpTransport::SendRtcp(
    std::span<const uint8_t> packet,
    const webrtc::PacketOptions& /*options*/)
{
    if (owner_ == nullptr || packet.empty()) {
        return false;
    }
    owner_->SendOutgoingRtcp(packet.data(), packet.size());
    return true;
}

webrtc::EncodedImageCallback::Result OmniEncoderSink::OnEncodedImage(
    const webrtc::EncodedImage& encoded_image,
    const webrtc::CodecSpecificInfo* codec_specific_info)
{
    if (owner_ == nullptr) {
        return webrtc::EncodedImageCallback::Result(
            webrtc::EncodedImageCallback::Result::ERROR_SEND_FAILED);
    }
    return owner_->OnEncodedImageReady(encoded_image, codec_specific_info);
}

void OmniEncoderSink::OnDroppedFrame(DropReason reason)
{
    (void)reason;
    if (owner_ != nullptr) {
        owner_->OnEncoderDroppedFrame();
    }
}

void OmniPlayoutReceiver::OnEncodedFrame(std::unique_ptr<webrtc::EncodedFrame> frame)
{
    if (owner_ == nullptr) {
        return;
    }
    owner_->OnPlayoutFrameReady(std::move(frame));
}

void OmniPlayoutReceiver::OnDecodableFrameTimeout(webrtc::TimeDelta wait_time)
{
    if (owner_ == nullptr) {
        return;
    }
    owner_->OnPlayoutDecodeTimeout(wait_time);
}

void OmniRtxMediaSink::OnRtpPacket(const webrtc::RtpPacketReceived& packet)
{
    if (owner_ == nullptr) {
        return;
    }
    // Копия нужна: апстрим отдаёт пакет по константной ссылке на свою локальную переменную, а
    // дальше по пути его читают и помечают как свой.
    webrtc::RtpPacketReceived recovered(packet);
    (void)owner_->HandleMediaRtpPacket(recovered);
}

bool OmniModuleTransport::SendRtp(
    std::span<const uint8_t> packet,
    const webrtc::PacketOptions& options)
{
    if (owner_ == nullptr || packet.empty()) {
        return false;
    }
    // Сначала отдать, потом сообщить: время отправки для регулятора снимается ПОСЛЕ того, как
    // пакет ушёл в ядро, а не до ожидания чужого такта, как было при очереди.
    if (!owner_->SendModuleRtpPacket(packet)) {
        return false;
    }
    // ФАКТ ОТПРАВКИ — ВТОРАЯ ПОЛОВИНА ОЦЕНКИ КАНАЛА, И ЕЁ НАДО СООБЩИТЬ.
    //
    // Первую половину (что пакет поставлен в очередь) сборщик узнаёт сам, через
    // RegisterNotifyBweCallback. Вторую — что пакет ушёл и КОГДА — узнать ему неоткуда: у
    // апстрима её приносит Call с сетевого потока. Без неё история отправки не получает времени,
    // и отчёт собеседника не с чем сопоставлять: ровно та поломка, что дала «ccfbApplied 79/0»
    // на перевороте #50.
    //
    // packet_id проставляет RtpSenderEgress из транспортного номера пакета
    // (rtp_sender_egress.cc:283); если его нет, сопоставлять нечего и сообщать не о чем.
    owner_->NotifyModulePacketSent(packet, options);
    return true;
}

bool OmniModuleTransport::SendRtcp(
    std::span<const uint8_t> packet,
    const webrtc::PacketOptions& /*options*/)
{
    if (owner_ == nullptr || packet.empty()) {
        return false;
    }
    owner_->SendOutgoingRtcp(packet.data(), packet.size());
    return true;
}

/// Av1QpParser against libaom itself: libaom reports base_q_idx of every frame it encodes
/// (AOME_GET_LAST_QUANTIZER -> EncodedImage::qp_), so the parser must read the same number out of
/// the same bytes — the key frame already encoded and delta frames of changing content, at
/// bitrates that move the quantizer. "ok N" or the first mismatch.
std::string CheckAv1QpParser(webrtc::VideoEncoder& encoder, SmokeEncodeCallback& callback,
                             const webrtc::scoped_refptr<webrtc::I420Buffer>& source, int fps)
{
    for (int i = 1; i <= 12; ++i) {
        webrtc::scoped_refptr<webrtc::I420Buffer> moved =
            webrtc::I420Buffer::Create(source->width(), source->height());
        for (int y = 0; y < source->height(); ++y) {
            for (int x = 0; x < source->width(); ++x) {
                moved->MutableDataY()[y * moved->StrideY() + x] = static_cast<uint8_t>(
                    source->DataY()[y * source->StrideY() + (x + i * 9) % source->width()] ^ ((x * y * i) & 0x1F));
            }
        }
        for (int y = 0; y < source->ChromaHeight(); ++y) {
            std::memcpy(moved->MutableDataU() + y * moved->StrideU(), source->DataU() + y * source->StrideU(),
                        static_cast<size_t>(source->ChromaWidth()));
            std::memcpy(moved->MutableDataV() + y * moved->StrideV(), source->DataV() + y * source->StrideV(),
                        static_cast<size_t>(source->ChromaWidth()));
        }
        webrtc::VideoBitrateAllocation bitrate;
        bitrate.SetBitrate(0, 0, i <= 6 ? 120'000 : 1'500'000);
        encoder.SetRates(webrtc::VideoEncoder::RateControlParameters(bitrate, fps));
        webrtc::VideoFrame frame = webrtc::VideoFrame::Builder()
            .set_video_frame_buffer(moved)
            .set_rtp_timestamp(90'000 + static_cast<uint32_t>(i) * 3000)
            .set_timestamp_us(1'000'000 + static_cast<int64_t>(i) * 33'333)
            .build();
        std::vector<webrtc::VideoFrameType> types{webrtc::VideoFrameType::kVideoFrameDelta};
        const size_t before = callback.FrameCount();
        if (encoder.Encode(frame, &types) != WEBRTC_VIDEO_CODEC_OK) {
            return "encode failed at frame " + std::to_string(i);
        }
        for (int wait = 0; wait < 75 && callback.FrameCount() == before; ++wait) {
            std::this_thread::sleep_for(std::chrono::milliseconds(10));
        }
    }
    Av1QpParser parser;
    int checked = 0;
    for (size_t i = 0; i < callback.frames.size(); ++i) {
        const auto& [bytes, reported] = callback.frames[i];
        const std::optional<int> parsed = parser.Parse(bytes.data(), bytes.size());
        if (reported < 0) {
            continue;
        }
        if (!parsed.has_value() || *parsed != reported) {
            return "MISMATCH frame " + std::to_string(i) + " libaom=" + std::to_string(reported) +
                " parsed=" + (parsed.has_value() ? std::to_string(*parsed) : std::string("none"));
        }
        ++checked;
    }
    return checked >= 8 ? "ok " + std::to_string(checked) : "too few frames " + std::to_string(checked);
}

CodecSmokeResult RunLibWebRtcRoundTripSmoke(NativeVideoCodec codec_kind)
{
    constexpr int kWidth = 320;
    constexpr int kHeight = 180;
    constexpr int kFps = 30;
    CodecSmokeResult result;
    const std::string codec_name(NativeVideoCodecName(codec_kind));

    webrtc::Environment env = MakeOmniEnvironment();
    std::unique_ptr<webrtc::VideoEncoderFactory> encoder_factory =
        webrtc::CreateBuiltinVideoEncoderFactory();
    std::unique_ptr<webrtc::VideoDecoderFactory> decoder_factory =
        webrtc::CreateBuiltinVideoDecoderFactory();
    if (!encoder_factory || !decoder_factory) {
        result.diagnostics = "codec factory creation failed";
        return result;
    }

    std::optional<webrtc::SdpVideoFormat> encoder_format =
        FindSupportedFormat(encoder_factory->GetSupportedFormats(), codec_kind);
    std::optional<webrtc::SdpVideoFormat> decoder_format =
        FindSupportedFormat(decoder_factory->GetSupportedFormats(), codec_kind);
    if (!encoder_format.has_value() || !decoder_format.has_value()) {
        result.diagnostics = codec_name + " encoder or decoder format is unavailable";
        return result;
    }

    std::unique_ptr<webrtc::VideoEncoder> encoder =
        encoder_factory->Create(env, *encoder_format);
    std::unique_ptr<webrtc::VideoDecoder> decoder =
        decoder_factory->Create(env, *decoder_format);
    if (!encoder || !decoder) {
        result.diagnostics = codec_name + " encoder or decoder creation failed";
        return result;
    }

    SmokeEncodeCallback encode_callback;
    if (encoder->RegisterEncodeCompleteCallback(&encode_callback) != WEBRTC_VIDEO_CODEC_OK) {
        result.diagnostics = codec_name + " encoder callback registration failed";
        return result;
    }

    webrtc::VideoCodec codec = BuildSmokeCodec(codec_kind, kWidth, kHeight);
    const webrtc::VideoEncoder::Capabilities capabilities(/*loss_notification=*/false);
    const webrtc::VideoEncoder::Settings settings(
        capabilities,
        1,
        1200);
    const int init_status = encoder->InitEncode(&codec, settings);
    if (init_status != WEBRTC_VIDEO_CODEC_OK) {
        result.diagnostics = codec_name + " encoder InitEncode failed: " + std::to_string(init_status);
        return result;
    }

    webrtc::VideoBitrateAllocation bitrate;
    bitrate.SetBitrate(0, 0, 500'000);
    encoder->SetRates(webrtc::VideoEncoder::RateControlParameters(bitrate, kFps));

    webrtc::scoped_refptr<webrtc::I420Buffer> source = webrtc::I420Buffer::Create(kWidth, kHeight);
    FillVerticalI420Bars(source);
    webrtc::VideoFrame frame = webrtc::VideoFrame::Builder()
        .set_video_frame_buffer(source)
        .set_rtp_timestamp(90'000)
        .set_timestamp_us(1'000'000)
        .build();
    std::vector<webrtc::VideoFrameType> frame_types{webrtc::VideoFrameType::kVideoFrameKey};
    const int encode_status = encoder->Encode(frame, &frame_types);
    if (encode_status == WEBRTC_VIDEO_CODEC_OK) {
        (void)encode_callback.WaitForFrame(std::chrono::milliseconds(750));
    }
    if (encode_status != WEBRTC_VIDEO_CODEC_OK || encode_callback.encoded_data.empty()) {
        result.diagnostics = codec_name + " Encode failed or returned no data: " + std::to_string(encode_status);
        return result;
    }

    SmokeDecodeCallback decode_callback;
    if (decoder->RegisterDecodeCompleteCallback(&decode_callback) != WEBRTC_VIDEO_CODEC_OK) {
        result.diagnostics = codec_name + " decoder callback registration failed";
        return result;
    }
    webrtc::VideoDecoder::Settings decoder_settings;
    decoder_settings.set_codec_type(NativeVideoCodecType(codec_kind));
    decoder_settings.set_max_render_resolution({kWidth, kHeight});
    decoder_settings.set_number_of_cores(1);
    if (!decoder->Configure(decoder_settings)) {
        result.diagnostics = codec_name + " decoder Configure failed";
        return result;
    }

    webrtc::EncodedImage encoded;
    encoded.SetEncodedData(webrtc::EncodedImageBuffer::Create(
        encode_callback.encoded_data.data(),
        encode_callback.encoded_data.size()));
    encoded.SetFrameType(encode_callback.frame_type);
    encoded.SetRtpTimestamp(frame.rtp_timestamp());
    const int decode_status = decoder->Decode(encoded, 0);
    if (decode_status == WEBRTC_VIDEO_CODEC_OK) {
        (void)decode_callback.WaitForFrame(std::chrono::milliseconds(750));
    }
    if (decode_status != WEBRTC_VIDEO_CODEC_OK || !decode_callback.decoded_i420) {
        result.diagnostics = codec_name + " Decode failed or returned no frame: " + std::to_string(decode_status);
        return result;
    }

    if (decode_callback.decoded_i420->width() != kWidth ||
        decode_callback.decoded_i420->height() != kHeight) {
        result.diagnostics = codec_name + " decoded frame dimensions mismatch";
        return result;
    }

    if (!DecodedColorBarsArePlausible(decode_callback.decoded_i420)) {
        result.diagnostics = codec_name + " color-bar roundtrip failed";
        return result;
    }

    result.ok = true;
    result.encoded_bytes = encode_callback.encoded_data.size();
    result.diagnostics = codec_name + " encode/decode color-bars ok";
    if (codec_kind == NativeVideoCodec::Av1) {
        result.diagnostics += "; qp_parse=" + CheckAv1QpParser(*encoder, encode_callback, source, kFps);
    }
    return result;
}

#if defined(_WIN32)
bool SmokeHardwareEncoder(NativeVideoCodec codec, std::string& note);

CodecSmokeResult RunMediaFoundationH264RoundTripSmoke()
{
    constexpr int kWidth = 320;
    constexpr int kHeight = 180;
    constexpr int kFps = 30;
    CodecSmokeResult result;

    std::string encoder_error;
    std::unique_ptr<MediaFoundationH264Encoder> encoder =
        MediaFoundationH264Encoder::Create(kWidth, kHeight, kFps, 700'000, encoder_error);
    if (!encoder) {
        result.diagnostics = encoder_error.empty()
            ? "Media Foundation hardware H.264 encoder creation failed"
            : encoder_error;
        return result;
    }

    webrtc::scoped_refptr<webrtc::I420Buffer> source = webrtc::I420Buffer::Create(kWidth, kHeight);
    FillVerticalI420Bars(source);
    const size_t y_size = static_cast<size_t>(kWidth) * static_cast<size_t>(kHeight);
    const size_t uv_size = static_cast<size_t>(kWidth) * static_cast<size_t>((kHeight + 1) / 2);
    std::vector<uint8_t> nv12(y_size + uv_size);
    const int convert_status = libyuv::I420ToNV12(
        source->DataY(),
        source->StrideY(),
        source->DataU(),
        source->StrideU(),
        source->DataV(),
        source->StrideV(),
        nv12.data(),
        kWidth,
        nv12.data() + y_size,
        kWidth,
        kWidth,
        kHeight);
    if (convert_status != 0) {
        result.diagnostics = "I420ToNV12 color-bars conversion failed";
        return result;
    }

    EncodedFrame encoded;
    for (int frame_index = 0; frame_index < 8 && encoded.data.empty(); ++frame_index) {
        const int64_t timestamp_us = static_cast<int64_t>(frame_index + 1) * 1'000'000 / kFps;
        const uint32_t rtp_timestamp = static_cast<uint32_t>((timestamp_us * 90) / 1000);
        const OmniWebRtcMediaStatus status = encoder->EncodeNv12(
            nv12.data(),
            kWidth,
            nv12.data() + y_size,
            kWidth,
            timestamp_us,
            frame_index == 0,
            rtp_timestamp,
            encoded);
        if (status != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            result.diagnostics = "Media Foundation hardware H.264 encode failed: " + encoder->LastError();
            return result;
        }
    }

    if (encoded.data.empty()) {
        result.diagnostics = "Media Foundation hardware H.264 encoder returned no frame";
        return result;
    }

    // The same proof as HEVC and AV1: the MFT keeps producing frames and gives a key frame when
    // asked. It used to decode the output with libwebrtc's software H.264 decoder, which the build
    // no longer has (per-pair-video-codec-selection.md §12).
    std::string keyframe_note;
    if (!SmokeHardwareEncoder(NativeVideoCodec::H264, keyframe_note)) {
        result.diagnostics = "Media Foundation hardware H.264 key frames on demand failed: " + keyframe_note;
        return result;
    }

    result.ok = true;
    result.encoded_bytes = encoded.data.size();
    result.diagnostics = "Media Foundation hardware H.264 encode ok via " + encoder->Name() + "; " +
        keyframe_note;
    return result;
}
#else
CodecSmokeResult RunMediaFoundationH264RoundTripSmoke()
{
    CodecSmokeResult result;
    result.diagnostics = "Media Foundation hardware H.264 smoke is Windows-only";
    return result;
}
#endif

struct HardwareH264ProbeResult {
    bool media_foundation_available = false;
    bool production_session = false;
    uint32_t encoder_count = 0;
    uint32_t decoder_count = 0;
    std::vector<std::string> encoder_names;
    std::vector<std::string> decoder_names;
    size_t encoded_bytes = 0;
    std::string reason;
};

#if defined(_WIN32)
HRESULT EnumerateHardwareH264Mfts(
    GUID category,
    const MFT_REGISTER_TYPE_INFO* input_type,
    const MFT_REGISTER_TYPE_INFO* output_type,
    std::vector<std::string>& names)
{
    IMFActivate** activates = nullptr;
    UINT32 activate_count = 0;
    const HRESULT hr = MFTEnumEx(
        category,
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        const_cast<MFT_REGISTER_TYPE_INFO*>(input_type),
        const_cast<MFT_REGISTER_TYPE_INFO*>(output_type),
        &activates,
        &activate_count);
    if (FAILED(hr)) {
        return hr;
    }

    names.clear();
    names.reserve(activate_count);
    for (UINT32 i = 0; i < activate_count; ++i) {
        IMFActivate* activate = activates[i];
        if (activate == nullptr) {
            continue;
        }

        wchar_t* friendly_name = nullptr;
        UINT32 friendly_name_length = 0;
        if (SUCCEEDED(activate->GetAllocatedString(
                MFT_FRIENDLY_NAME_Attribute,
                &friendly_name,
                &friendly_name_length)) &&
            friendly_name != nullptr &&
            friendly_name_length > 0) {
            std::string name = WideToUtf8(friendly_name);
            if (!name.empty()) {
                names.push_back(std::move(name));
            }
        }

        if (friendly_name != nullptr) {
            CoTaskMemFree(friendly_name);
        }

        activate->Release();
    }

    CoTaskMemFree(activates);
    return S_OK;
}

HardwareH264ProbeResult ProbeHardwareH264()
{
    HardwareH264ProbeResult result;
    const HRESULT startup_hr = MFStartup(MF_VERSION, MFSTARTUP_LITE);
    if (FAILED(startup_hr)) {
        std::ostringstream reason;
        reason << "Media Foundation startup failed hr=0x"
               << std::hex << static_cast<uint32_t>(startup_hr);
        result.reason = reason.str();
        return result;
    }

    result.media_foundation_available = true;
    MFT_REGISTER_TYPE_INFO h264_type;
    h264_type.guidMajorType = MFMediaType_Video;
    h264_type.guidSubtype = MFVideoFormat_H264;

    const HRESULT encoder_hr = EnumerateHardwareH264Mfts(
        MFT_CATEGORY_VIDEO_ENCODER,
        nullptr,
        &h264_type,
        result.encoder_names);
    const HRESULT decoder_hr = EnumerateHardwareH264Mfts(
        MFT_CATEGORY_VIDEO_DECODER,
        &h264_type,
        nullptr,
        result.decoder_names);

    result.encoder_count = static_cast<uint32_t>(result.encoder_names.size());
    result.decoder_count = static_cast<uint32_t>(result.decoder_names.size());
    if (SUCCEEDED(encoder_hr) && SUCCEEDED(decoder_hr)) {
        const CodecSmokeResult smoke = RunMediaFoundationH264RoundTripSmoke();
        result.production_session = smoke.ok;
        result.encoded_bytes = smoke.encoded_bytes;
        if (smoke.ok) {
            result.reason = "Media Foundation hardware H.264 production encoder smoke passed: " +
                smoke.diagnostics;
        } else {
            result.reason = "Media Foundation hardware H.264 production encoder smoke failed: " +
                smoke.diagnostics;
        }
    } else {
        std::ostringstream reason;
        reason << "Media Foundation hardware H.264 MFT enumeration failed"
               << " encoder_hr=0x" << std::hex << static_cast<uint32_t>(encoder_hr)
               << " decoder_hr=0x" << std::hex << static_cast<uint32_t>(decoder_hr);
        result.reason = reason.str();
    }

    (void)MFShutdown();
    return result;
}
#else
HardwareH264ProbeResult ProbeHardwareH264()
{
    HardwareH264ProbeResult result;
    result.reason = "Media Foundation hardware H.264 probe is Windows-only";
    return result;
}
#endif

std::string HardwareH264ProbeJson(const HardwareH264ProbeResult& result)
{
    std::ostringstream json;
    json
        << R"({"media_foundation_available":)" << (result.media_foundation_available ? "true" : "false")
        << R"(,"encoder_count":)" << result.encoder_count
        << R"(,"decoder_count":)" << result.decoder_count
        << R"(,"encoder_names":)" << JsonArray(result.encoder_names)
        << R"(,"decoder_names":)" << JsonArray(result.decoder_names)
        << R"(,"production_session":)" << (result.production_session ? "true" : "false")
        << R"(,"encoded_bytes":)" << result.encoded_bytes
        << R"(,"reason":")" << JsonEscape(result.reason) << R"("})";
    return json.str();
}

// One real GPU (DXGI adapter) and what it can actually do for H.264, both directions. Decode is a
// genuine D3D11 video-device profile check (this is also what fixes the phantom dec:0 the old
// HARDWARE-flag MFT enumeration reported). Encode is a *functional* test: the same production
// encoder is created with a vendor filter and asked to encode a frame — so e.g. Intel QSV, which
// configures but fails ProcessOutput (0x8000ffff), is honestly reported as can_encode=false.
struct VideoDeviceCapability {
    std::string description;
    uint32_t vendor_id = 0;
    bool integrated = false;
    bool can_encode_h264 = false;
    bool can_decode_h264 = false;
};

#if defined(_WIN32)
bool ProbeAdapterCanEncodeH264(UINT vendor_id)
{
    std::string error;
    std::unique_ptr<MediaFoundationH264Encoder> encoder =
        MediaFoundationH264Encoder::Create(320, 240, 30, 1'000'000, error, vendor_id);
    if (!encoder) {
        return false;
    }

    constexpr int kWidth = 320;
    constexpr int kHeight = 240;
    std::vector<uint8_t> y(static_cast<size_t>(kWidth) * kHeight, 16);          // black luma
    std::vector<uint8_t> uv(static_cast<size_t>(kWidth) * (kHeight / 2), 128);  // neutral chroma
    for (int i = 0; i < 24; ++i) {
        EncodedFrame encoded;
        const OmniWebRtcMediaStatus status = encoder->EncodeNv12(
            y.data(), kWidth, uv.data(), kWidth,
            static_cast<int64_t>(i) * 33'000, i == 0,
            static_cast<uint32_t>(i) * 3000u, encoded);
        if (status != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            return false;  // e.g. QSV ProcessOutput 0x8000ffff — not actually functional
        }
        if (!encoded.data.empty()) {
            return true;  // produced a real encoded frame — this adapter can HW-encode H.264
        }
    }
    return false;  // never emitted a frame within the warm-up window
}

bool AdapterCanDecodeH264(IDXGIAdapter1* adapter)
{
    Microsoft::WRL::ComPtr<ID3D11Device> device;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context;
    D3D_FEATURE_LEVEL feature_level = D3D_FEATURE_LEVEL_11_0;
    const HRESULT hr = ::D3D11CreateDevice(
        adapter, D3D_DRIVER_TYPE_UNKNOWN, nullptr,
        D3D11_CREATE_DEVICE_VIDEO_SUPPORT, nullptr, 0,
        D3D11_SDK_VERSION, device.GetAddressOf(), &feature_level, context.GetAddressOf());
    if (FAILED(hr) || !device) {
        return false;
    }

    Microsoft::WRL::ComPtr<ID3D11VideoDevice> video_device;
    if (FAILED(device.As(&video_device)) || !video_device) {
        return false;
    }

    // Well-known DXVA H.264 VLD decode-profile GUIDs, defined locally so we don't depend on the
    // dxguid.lib / INITGUID symbol (the d3d11.h macros are extern-declared and don't link here).
    static const GUID kH264VldNoFgt = {
        0x1b81be68, 0xa0c7, 0x11d3, {0xb9, 0x84, 0x00, 0xc0, 0x4f, 0x2e, 0x73, 0xc5}};
    static const GUID kH264VldFgt = {
        0x1b81be69, 0xa0c7, 0x11d3, {0xb9, 0x84, 0x00, 0xc0, 0x4f, 0x2e, 0x73, 0xc5}};

    const UINT profile_count = video_device->GetVideoDecoderProfileCount();
    for (UINT i = 0; i < profile_count; ++i) {
        GUID profile = {};
        if (SUCCEEDED(video_device->GetVideoDecoderProfile(i, &profile)) &&
            (IsEqualGUID(profile, kH264VldNoFgt) || IsEqualGUID(profile, kH264VldFgt))) {
            return true;
        }
    }
    return false;
}

std::vector<VideoDeviceCapability> ProbeVideoDevices()
{
    std::vector<VideoDeviceCapability> devices;
    Microsoft::WRL::ComPtr<IDXGIFactory1> factory;
    if (FAILED(::CreateDXGIFactory1(IID_PPV_ARGS(factory.GetAddressOf())))) {
        return devices;
    }

    Microsoft::WRL::ComPtr<IDXGIAdapter1> adapter;
    for (UINT i = 0;
         factory->EnumAdapters1(i, adapter.ReleaseAndGetAddressOf()) != DXGI_ERROR_NOT_FOUND;
         ++i) {
        DXGI_ADAPTER_DESC1 desc = {};
        if (FAILED(adapter->GetDesc1(&desc))) {
            continue;
        }
        if ((desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE) != 0) {
            continue;  // skip the Microsoft Basic Render Driver / WARP
        }

        VideoDeviceCapability cap;
        cap.description = WideToUtf8(desc.Description);
        cap.vendor_id = desc.VendorId;
        // iGPUs share system RAM (little/no dedicated VRAM); a dGPU like the RTX 4060 reports GBs.
        cap.integrated = desc.DedicatedVideoMemory < (512ull * 1024 * 1024);
        cap.can_decode_h264 = AdapterCanDecodeH264(adapter.Get());
        cap.can_encode_h264 = ProbeAdapterCanEncodeH264(desc.VendorId);
        devices.push_back(std::move(cap));
    }

    return devices;
}
#else
std::vector<VideoDeviceCapability> ProbeVideoDevices()
{
    return {};
}
#endif

std::string VideoDevicesJson(const std::vector<VideoDeviceCapability>& devices)
{
    std::ostringstream json;
    json << '[';
    for (size_t i = 0; i < devices.size(); ++i) {
        const VideoDeviceCapability& device = devices[i];
        if (i != 0) {
            json << ',';
        }
        json
            << R"({"description":")" << JsonEscape(device.description) << '"'
            << R"(,"vendor_id":)" << device.vendor_id
            << R"(,"integrated":)" << (device.integrated ? "true" : "false")
            << R"(,"can_encode_h264":)" << (device.can_encode_h264 ? "true" : "false")
            << R"(,"can_decode_h264":)" << (device.can_decode_h264 ? "true" : "false")
            << '}';
    }
    json << ']';
    return json.str();
}

#if defined(_WIN32)
/// A hardware encoder counts only if it really encodes here: open the MFT, push a few frames, get
/// a key frame and bytes back (docs/decisions/per-pair-video-codec-selection.md §2 — "проверяются
/// на машине": a driver that lists AV1 may still refuse a session).
bool SmokeHardwareEncoder(NativeVideoCodec codec, std::string& note)
{
    constexpr int kWidth = 320;
    constexpr int kHeight = 240;
    std::string error;
    std::unique_ptr<MediaFoundationH264Encoder> encoder = MediaFoundationH264Encoder::Create(
        kWidth, kHeight, 30, 600'000, error, 0, codec);
    if (!encoder) {
        note = error;
        return false;
    }
    std::vector<uint8_t> y(static_cast<size_t>(kWidth * kHeight));
    std::vector<uint8_t> uv(static_cast<size_t>(kWidth * kHeight / 2), 128);
    size_t bytes = 0;
    bool keyframe = false;
    std::string shape;
    std::string keys;
    std::string qps;
    int width = kWidth;
    for (int i = 0; i < 12; ++i) {
        // Resize in place at frame 8, as the call does when quality scaling steps down.
        if (i == 8) {
            std::string resize_error;
            width = 256;
            if (!encoder->ReconfigureResolution(width, 192, resize_error)) {
                note = "resize failed: " + resize_error;
                return false;
            }
        }
        for (size_t p = 0; p < y.size(); ++p) {
            y[p] = static_cast<uint8_t>((p + static_cast<size_t>(i) * 7) & 0xFF);
        }
        EncodedFrame frame;
        const OmniWebRtcMediaStatus status = encoder->EncodeNv12(
            y.data(), width, uv.data(), width, static_cast<int64_t>(i) * 33'333, i == 0 || i == 6,
            static_cast<uint32_t>(i) * 3000, frame);
        if (status != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            note = encoder->Name() + " encode failed";
            return false;
        }
        bytes += frame.data.size();
        keyframe = keyframe || frame.frame_type == webrtc::VideoFrameType::kVideoFrameKey;
        if (frame.frame_type == webrtc::VideoFrameType::kVideoFrameKey) {
            keys += " " + std::to_string(i);
        }
        if (codec == NativeVideoCodec::Av1 && !frame.data.empty()) {
            qps += frame.qp.has_value() ? " " + std::to_string(*frame.qp) : std::string(" -");
        }
        // What the bitstream looks like (first frames): key flag and leading bytes.
        if ((i < 2 || i == 6 || i == 8 || i == 9) && !frame.data.empty()) {
            char head[64];
            const size_t n = std::min<size_t>(frame.data.size(), 8);
            int written = std::snprintf(head, sizeof(head), " f%d%s:", i,
                                        frame.frame_type == webrtc::VideoFrameType::kVideoFrameKey ? "K" : "d");
            for (size_t b = 0; b < n && written > 0 && written < 60; ++b) {
                written += std::snprintf(head + written, sizeof(head) - written, "%02x", frame.data[b]);
            }
            shape += head;
            if (codec == NativeVideoCodec::H265) {
                shape += "[";
                for (const auto& nalu : webrtc::H264::FindNaluIndices(frame.data)) {
                    if (nalu.payload_size > 0 && nalu.payload_start_offset < frame.data.size()) {
                        shape += std::to_string((frame.data[nalu.payload_start_offset] >> 1) & 0x3F) + ",";
                    }
                }
                shape += "]";
            }
        }
    }
    if (bytes == 0 || !keyframe) {
        note = encoder->Name() + (bytes == 0 ? " produced no output" : " produced no key frame");
        return false;
    }
    note = encoder->Name() + " ok " + std::to_string(bytes) + "B keys(asked 0,6):" + keys + shape
        + (codec == NativeVideoCodec::Av1 ? " qp:" + qps : std::string());
    RememberVerifiedHardwareEncoder(codec, encoder->Name());
    return true;
}
#endif

#if defined(_WIN32)
/// A hardware decoder counts only if it decodes here: our own hardware encoder makes the stream,
/// the Media Foundation decoder must turn it into a picture on a D3D11 device (the render path).
/// A machine that decodes HEVC but cannot encode it cannot prove it this way and does not claim it.
bool SmokeHardwareDecoder(NativeVideoCodec codec, std::string& note)
{
    constexpr int kWidth = 320;
    constexpr int kHeight = 192;
    Microsoft::WRL::ComPtr<ID3D11Device> device;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context;
    const UINT flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
    D3D_FEATURE_LEVEL feature_level = D3D_FEATURE_LEVEL_11_1;
    if (FAILED(::D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, flags, nullptr, 0,
                                   D3D11_SDK_VERSION, device.GetAddressOf(), &feature_level,
                                   context.GetAddressOf())) ||
        !device) {
        note = "no D3D11 video device";
        return false;
    }
    Microsoft::WRL::ComPtr<ID3D10Multithread> multithread;
    if (SUCCEEDED(device.As(&multithread)) && multithread) {
        multithread->SetMultithreadProtected(TRUE);
    }
    std::string error;
    auto encoder = MediaFoundationH264Encoder::Create(kWidth, kHeight, 30, 800'000, error, 0, codec);
    if (!encoder) {
        note = "no encoder to make a test stream: " + error;
        return false;
    }
    auto decoder = MediaFoundationH264Decoder::Create(device.Get(), kWidth, kHeight, error, codec);
    if (!decoder) {
        note = error;
        return false;
    }
    std::vector<uint8_t> y(static_cast<size_t>(kWidth * kHeight));
    std::vector<uint8_t> uv(static_cast<size_t>(kWidth * kHeight / 2), 128);
    Microsoft::WRL::ComPtr<ID3D11Texture2D> nv12;
    UINT subresource = 0;
    bool produced = false;
    for (int i = 0; i < 24 && !produced; ++i) {
        for (size_t p = 0; p < y.size(); ++p) {
            y[p] = static_cast<uint8_t>((p / kWidth + static_cast<size_t>(i) * 3) & 0xFF);
        }
        EncodedFrame frame;
        if (encoder->EncodeNv12(y.data(), kWidth, uv.data(), kWidth, static_cast<int64_t>(i) * 33'333,
                                i == 0, static_cast<uint32_t>(i) * 3000, frame) !=
                OMNI_WEBRTC_MEDIA_STATUS_OK ||
            frame.data.empty()) {
            continue;
        }
        if (decoder->Decode(frame.data.data(), frame.data.size(), static_cast<int64_t>(i) * 333'333,
                            nv12, subresource, produced) != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            note = "decode failed";
            return false;
        }
    }
    if (!produced || !nv12) {
        note = "decoder produced no picture";
        return false;
    }
    note = "ok";
    return true;
}
#endif

std::string BuildLibWebRtcProbeJson()
{
    const std::unique_ptr<webrtc::VideoEncoderFactory> encoder_factory =
        webrtc::CreateBuiltinVideoEncoderFactory();
    const std::unique_ptr<webrtc::VideoDecoderFactory> decoder_factory =
        webrtc::CreateBuiltinVideoDecoderFactory();

    if (!encoder_factory || !decoder_factory) {
        return R"({"abi_version":23,"available":false,"backend":"libwebrtc","codec_factory_probe_available":false,"transport":"omni_quic_datagram_control","supports_peer_connection":false,"supports_rtp_over_omni_quic":false,"supports_rtcp_feedback":false,"hardware_codecs":[],"software_codecs":[],"reason":"libwebrtc codec factory creation failed"})";
    }

    const std::set<std::string> encoder_codecs =
        CollectCodecNames(encoder_factory->GetSupportedFormats());
    const std::set<std::string> decoder_codecs =
        CollectCodecNames(decoder_factory->GetSupportedFormats());
    const std::set<std::string> software_codecs =
        IntersectCodecs(encoder_codecs, decoder_codecs);
    // Software H.264 is not in the build; the smoke runs only for a build that still has it.
    const CodecSmokeResult h264_smoke = software_codecs.count("h264") != 0
        ? RunLibWebRtcRoundTripSmoke(NativeVideoCodec::H264)
        : CodecSmokeResult{};
    const HardwareH264ProbeResult hardware_h264_probe = ProbeHardwareH264();
    const std::vector<VideoDeviceCapability> video_devices = ProbeVideoDevices();
    // Кодек доступен, если работает ЛЮБОЙ путь — аппаратный или программный.
    //
    // Раньше список наполнялся только при удачной аппаратной сессии, и `available` вместе с ним.
    // На машине без исправного GPU-кодировщика это выключало видео целиком, хотя программный
    // H.264 проверялся строкой выше и исправно проходил — он шёл лишь в диагностику. Теперь
    // сессия умеет кодировать процессором (см. откат в `EnsureEncoder`), поэтому и проба обязана
    // об этом говорить. См. docs/bugs/hardware-h264-never-started.md.
    std::set<std::string> native_session_codecs;
    if (hardware_h264_probe.production_session || h264_smoke.ok) {
        native_session_codecs.insert("h264");
    }
    // AV1 в программном виде (libaom кодирует, dav1d декодирует) — тем же прогоном цветных полос,
    // что и H.264: сессия везёт кодек, только если он реально прошёл кодирование и декодирование
    // здесь (docs/decisions/per-pair-video-codec-selection.md §2: «проверяются на машине»).
    const CodecSmokeResult av1_smoke = software_codecs.count("av1") != 0
        ? RunLibWebRtcRoundTripSmoke(NativeVideoCodec::Av1)
        : CodecSmokeResult{};
    if (av1_smoke.ok) {
        native_session_codecs.insert("av1");
    }
    // А ЭТОТ список остаётся строго аппаратным: он отвечает на другой вопрос — «что умеет
    // железо», и смешивать в нём программный кодек значило бы врать в диагностике.
    // `hardware_codecs` — аппаратное КОДИРОВАНИЕ; декодирование — отдельный список ниже.
    std::set<std::string> hardware_codecs;
    std::set<std::string> hardware_decode_codecs;
    if (hardware_h264_probe.production_session) {
        hardware_codecs.insert("h264");
    }
    std::string av1_hardware_note = "not probed";
    std::string h265_hardware_note = "not probed";
#if defined(_WIN32)
    // H.264, like HEVC, decodes only through Media Foundation: offered for receiving only where
    // that decoder really decodes here.
    std::string h264_decode_note = "not probed";
    if (SmokeHardwareDecoder(NativeVideoCodec::H264, h264_decode_note)) {
        hardware_decode_codecs.insert("h264");
        native_session_codecs.insert("h264");
    }
    // AV1 decodes in software everywhere; the hardware decoder is the cheaper way where it works.
    std::string av1_decode_note = "not probed";
    if (SmokeHardwareDecoder(NativeVideoCodec::Av1, av1_decode_note)) {
        hardware_decode_codecs.insert("av1");
    }
    std::string h265_decode_note = "not probed";
    if (SmokeHardwareDecoder(NativeVideoCodec::H265, h265_decode_note)) {
        hardware_decode_codecs.insert("h265");
        native_session_codecs.insert("h265");
    }
    h265_hardware_note += "; h265_decode=" + h265_decode_note + "; h264_decode=" + h264_decode_note +
        "; av1_decode=" + av1_decode_note;
    if (SmokeHardwareEncoder(NativeVideoCodec::Av1, av1_hardware_note)) {
        hardware_codecs.insert("av1");
        native_session_codecs.insert("av1");
    }
    if (SmokeHardwareEncoder(NativeVideoCodec::H265, h265_hardware_note)) {
        hardware_codecs.insert("h265");
        native_session_codecs.insert("h265");
    }
#endif
    const bool native_session_scaffold = !native_session_codecs.empty();
    const std::string smoke_diagnostics =
        "h264_software=" + h264_smoke.diagnostics +
        "; h264_hardware=" + hardware_h264_probe.reason +
        "; av1_software=" + (av1_smoke.diagnostics.empty() ? std::string("not built") : av1_smoke.diagnostics) +
        "; av1_hardware=" + av1_hardware_note + "; h265_hardware=" + h265_hardware_note;

    std::ostringstream json;
    json
        << R"({"abi_version":)" << OMNI_WEBRTC_MEDIA_ABI_VERSION
        << R"(,"available":)" << (native_session_scaffold ? "true" : "false")
        << R"(,"backend":"libwebrtc")"
        << R"(,"codec_factory_probe_available":true)"
        << R"(,"transport":"omni_quic_datagram_control")"
        << R"(,"supports_peer_connection":false)"
        << R"(,"supports_rtp_over_omni_quic":true)"
        << R"(,"supports_rtcp_feedback":true)"
        << R"(,"native_session_scaffold":)" << (native_session_scaffold ? "true" : "false")
        << R"(,"native_session_scaffold_codec":"h264")"
        << R"(,"native_session_scaffold_codecs":)" << JsonArray(native_session_codecs)
        << R"(,"native_session_nv12_input":true)"
        << R"(,"native_session_bgra_input":true)"
        << R"(,"native_session_bgra_output":true)"
        << R"(,"codec_roundtrip_smoke":)" << (h264_smoke.ok ? "true" : "false")
        << R"(,"hardware_codecs":)" << JsonArray(hardware_codecs)
        << R"(,"hardware_decode_codecs":)" << JsonArray(hardware_decode_codecs)
        << R"(,"hardware_h264_probe":)" << HardwareH264ProbeJson(hardware_h264_probe)
        << R"(,"video_devices":)" << VideoDevicesJson(video_devices)
        << R"(,"software_codecs":)" << JsonArray(software_codecs)
        << R"(,"encoder_codecs":)" << JsonArray(encoder_codecs)
        << R"(,"decoder_codecs":)" << JsonArray(decoder_codecs)
        << R"(,"codec_roundtrip_encoded_bytes":)" << h264_smoke.encoded_bytes
        << R"(,"codec_roundtrip_diagnostics":")" << JsonEscape(smoke_diagnostics) << '"'
        << R"(,"reason":"libwebrtc codec factories and bounded RTP session scaffold are linked")"
        << '}';
    return json.str();
}
#endif

std::string BuildProbeJson()
{
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    return BuildLibWebRtcProbeJson();
#else
    return std::string(kProbeJson);
#endif
}

} // namespace

extern "C" uint32_t omni_webrtc_media_abi_version(void)
{
    return OMNI_WEBRTC_MEDIA_ABI_VERSION;
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_probe_json(
    char* buffer,
    size_t buffer_len,
    size_t* required_len)
{
    const std::string probe_json = BuildProbeJson();
    return CopyString(probe_json, buffer, buffer_len, required_len);
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_session_create(
    const char* config_json,
    OmniWebRtcMediaSession** out_session,
    char* error_buffer,
    size_t error_buffer_len,
    size_t* required_error_len)
{
    if (out_session == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }

    *out_session = nullptr;

#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    std::string error;
    std::unique_ptr<LibWebRtcVideoSession> impl =
        LibWebRtcVideoSession::Create(config_json == nullptr ? "{}" : config_json, error);
    if (!impl) {
        (void)CopyString(error, error_buffer, error_buffer_len, required_error_len);
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }

    OmniWebRtcMediaSession* session = new OmniWebRtcMediaSession();
    session->impl = impl.release();
    *out_session = session;
    if (required_error_len != nullptr) {
        *required_error_len = 0;
    }
    if (error_buffer != nullptr && error_buffer_len > 0) {
        error_buffer[0] = '\0';
    }
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    (void)config_json;
    const OmniWebRtcMediaStatus copy_status = CopyString(
        kNoBackendSessionNotImplemented,
        error_buffer,
        error_buffer_len,
        required_error_len);
    return copy_status == OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR
        ? copy_status
        : OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" void omni_webrtc_media_session_destroy(OmniWebRtcMediaSession* session)
{
    if (session == nullptr) {
        return;
    }

#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    delete reinterpret_cast<LibWebRtcVideoSession*>(session->impl);
#endif
    delete session;
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_session_push_i420_frame(
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
    int32_t force_keyframe)
{
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (session == nullptr || session->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }

    return reinterpret_cast<LibWebRtcVideoSession*>(session->impl)->PushI420Frame(
        y_plane,
        y_stride,
        u_plane,
        u_stride,
        v_plane,
        v_stride,
        width,
        height,
        timestamp_us,
        force_keyframe);
#else
    (void)session;
    (void)y_plane;
    (void)y_stride;
    (void)u_plane;
    (void)u_stride;
    (void)v_plane;
    (void)v_stride;
    (void)width;
    (void)height;
    (void)timestamp_us;
    (void)force_keyframe;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_session_push_nv12_frame(
    OmniWebRtcMediaSession* session,
    const uint8_t* y_plane,
    int32_t y_stride,
    const uint8_t* uv_plane,
    int32_t uv_stride,
    int32_t width,
    int32_t height,
    int64_t timestamp_us,
    int32_t force_keyframe)
{
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (session == nullptr || session->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }

    return reinterpret_cast<LibWebRtcVideoSession*>(session->impl)->PushNv12Frame(
        y_plane,
        y_stride,
        uv_plane,
        uv_stride,
        width,
        height,
        timestamp_us,
        force_keyframe);
#else
    (void)session;
    (void)y_plane;
    (void)y_stride;
    (void)uv_plane;
    (void)uv_stride;
    (void)width;
    (void)height;
    (void)timestamp_us;
    (void)force_keyframe;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_session_push_bgra_frame(
    OmniWebRtcMediaSession* session,
    const uint8_t* bgra,
    int32_t bgra_stride,
    int32_t width,
    int32_t height,
    int64_t timestamp_us,
    int32_t force_keyframe)
{
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (session == nullptr || session->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }

    return reinterpret_cast<LibWebRtcVideoSession*>(session->impl)->PushBgraFrame(
        bgra,
        bgra_stride,
        width,
        height,
        timestamp_us,
        force_keyframe);
#else
    (void)session;
    (void)bgra;
    (void)bgra_stride;
    (void)width;
    (void)height;
    (void)timestamp_us;
    (void)force_keyframe;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_session_set_rates(
    OmniWebRtcMediaSession* session,
    int32_t bitrate_bps,
    int32_t fps)
{
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (session == nullptr || session->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }

    return reinterpret_cast<LibWebRtcVideoSession*>(session->impl)->SetRates(bitrate_bps, fps);
#else
    (void)session;
    (void)bitrate_bps;
    (void)fps;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_session_set_max_bitrate(
    OmniWebRtcMediaSession* session,
    int32_t max_bitrate_bps)
{
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (session == nullptr || session->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }

    return reinterpret_cast<LibWebRtcVideoSession*>(session->impl)->SetMaxBitrate(max_bitrate_bps);
#else
    (void)session;
    (void)max_bitrate_bps;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_session_request_keyframe(
    OmniWebRtcMediaSession* session)
{
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (session == nullptr || session->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }

    return reinterpret_cast<LibWebRtcVideoSession*>(session->impl)->RequestKeyframe();
#else
    (void)session;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}


extern "C" OmniWebRtcMediaStatus omni_webrtc_media_session_set_packet_sink(
    OmniWebRtcMediaSession* session,
    OmniWebRtcMediaPacketSink sink,
    void* context)
{
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (session == nullptr || session->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
    reinterpret_cast<LibWebRtcVideoSession*>(session->impl)->SetPacketSink(sink, context);
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    (void)session;
    (void)sink;
    (void)context;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_session_push_rtp_packets(
    OmniWebRtcMediaSession* session,
    const uint8_t* buffer,
    size_t buffer_len,
    uint32_t packet_count)
{
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (session == nullptr || session->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }

    return reinterpret_cast<LibWebRtcVideoSession*>(session->impl)->PushRtpPackets(
        buffer,
        buffer_len,
        packet_count);
#else
    (void)session;
    (void)buffer;
    (void)buffer_len;
    (void)packet_count;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}





extern "C" OmniWebRtcMediaStatus omni_webrtc_media_session_transport_rtt_us(
    OmniWebRtcMediaSession* session,
    int64_t* out_rtt_us)
{
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (session == nullptr || session->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
    return reinterpret_cast<LibWebRtcVideoSession*>(session->impl)->TransportRttUs(out_rtt_us);
#else
    (void)session;
    if (out_rtt_us != nullptr) {
        *out_rtt_us = -1;
    }
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}


extern "C" OmniWebRtcMediaStatus omni_webrtc_media_session_apply_transport_feedback(
    OmniWebRtcMediaSession* session,
    const uint8_t* data,
    size_t data_len)
{
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (session == nullptr || session->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }

    return reinterpret_cast<LibWebRtcVideoSession*>(session->impl)
        ->ApplyTransportFeedback(data, data_len);
#else
    (void)session;
    (void)data;
    (void)data_len;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

// #5 increment 3a (2026-06-01): GPU video renderer session FFI. The C# layer attaches a
// SwapChainPanel-backed swapchain, then drives RenderAttachedVideo on its render tick. Decoded
// frames render straight to the swapchain (no BGRA round-trip across FFI). Additive — no ABI
// bump (only new exports). NOT_IMPLEMENTED without libwebrtc+_WIN32.
extern "C" OmniWebRtcMediaStatus omni_webrtc_media_session_attach_video_renderer(
    OmniWebRtcMediaSession* session,
    int32_t width,
    int32_t height,
    void** out_swapchain)
{
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC) && defined(_WIN32)
    if (session == nullptr || session->impl == nullptr || out_swapchain == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
    return reinterpret_cast<LibWebRtcVideoSession*>(session->impl)->AttachVideoRenderer(
        width, height, out_swapchain);
#else
    (void)session;
    (void)width;
    (void)height;
    (void)out_swapchain;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_session_render_attached_video(
    OmniWebRtcMediaSession* session,
    bool* out_presented)
{
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC) && defined(_WIN32)
    if (session == nullptr || session->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
    return reinterpret_cast<LibWebRtcVideoSession*>(session->impl)
        ->RenderAttachedVideo(out_presented);
#else
    (void)session;
    if (out_presented != nullptr) {
        *out_presented = false;
    }
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_session_resize_attached_video(
    OmniWebRtcMediaSession* session,
    int32_t width,
    int32_t height)
{
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC) && defined(_WIN32)
    if (session == nullptr || session->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
    return reinterpret_cast<LibWebRtcVideoSession*>(session->impl)->ResizeAttachedVideo(width, height);
#else
    (void)session;
    (void)width;
    (void)height;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_session_detach_video_renderer(
    OmniWebRtcMediaSession* session)
{
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC) && defined(_WIN32)
    if (session == nullptr || session->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
    reinterpret_cast<LibWebRtcVideoSession*>(session->impl)->DetachVideoRenderer();
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    (void)session;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_session_update_abr(
    OmniWebRtcMediaSession* session,
    const OmniWebRtcMediaAbrSample* sample,
    OmniWebRtcMediaAbrTarget* target)
{
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (session == nullptr || session->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }

    return reinterpret_cast<LibWebRtcVideoSession*>(session->impl)->UpdateAbr(sample, target);
#else
    (void)session;
    (void)sample;
    if (target != nullptr) {
        std::memset(target, 0, sizeof(*target));
    }
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_session_drain_i420_frame(
    OmniWebRtcMediaSession* session,
    uint8_t* buffer,
    size_t buffer_len,
    int32_t* width,
    int32_t* height,
    uint32_t* rtp_timestamp,
    int64_t* timestamp_us,
    size_t* required_len)
{
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (session == nullptr || session->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }

    return reinterpret_cast<LibWebRtcVideoSession*>(session->impl)->DrainI420Frame(
        buffer,
        buffer_len,
        width,
        height,
        rtp_timestamp,
        timestamp_us,
        required_len);
#else
    (void)session;
    (void)buffer;
    (void)buffer_len;
    if (width != nullptr) {
        *width = 0;
    }
    if (height != nullptr) {
        *height = 0;
    }
    if (rtp_timestamp != nullptr) {
        *rtp_timestamp = 0;
    }
    if (timestamp_us != nullptr) {
        *timestamp_us = 0;
    }
    if (required_len != nullptr) {
        *required_len = 0;
    }
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_session_drain_bgra_frame(
    OmniWebRtcMediaSession* session,
    uint8_t* buffer,
    size_t buffer_len,
    int32_t* width,
    int32_t* height,
    uint32_t* rtp_timestamp,
    int64_t* timestamp_us,
    size_t* required_len)
{
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (session == nullptr || session->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }

    return reinterpret_cast<LibWebRtcVideoSession*>(session->impl)->DrainBgraFrame(
        buffer,
        buffer_len,
        width,
        height,
        rtp_timestamp,
        timestamp_us,
        required_len);
#else
    (void)session;
    (void)buffer;
    (void)buffer_len;
    if (width != nullptr) {
        *width = 0;
    }
    if (height != nullptr) {
        *height = 0;
    }
    if (rtp_timestamp != nullptr) {
        *rtp_timestamp = 0;
    }
    if (timestamp_us != nullptr) {
        *timestamp_us = 0;
    }
    if (required_len != nullptr) {
        *required_len = 0;
    }
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_session_diagnostics_json(
    OmniWebRtcMediaSession* session,
    char* buffer,
    size_t buffer_len,
    size_t* required_len)
{
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (session == nullptr || session->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }

    const std::string diagnostics =
        reinterpret_cast<LibWebRtcVideoSession*>(session->impl)->DiagnosticsJson();
    return CopyString(diagnostics, buffer, buffer_len, required_len);
#else
    (void)session;
    return CopyString(kNoBackendSessionNotImplemented, buffer, buffer_len, required_len) ==
            OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR
        ? OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR
        : OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

// =========================================================================
// Wave 3 (2026-05-16): libyuv-backed pixel ops.
// Replaces ~225 MB/s of managed bilinear in CallVideoFrameScaler.cs with
// SSE2/AVX2 paths inside libyuv. Bilinear filter throughout.
// =========================================================================
extern "C" OmniWebRtcMediaStatus omni_webrtc_media_scale_bgra(
    const uint8_t* src_bgra,
    int32_t src_stride,
    int32_t src_width,
    int32_t src_height,
    uint8_t* dst_bgra,
    int32_t dst_stride,
    int32_t dst_width,
    int32_t dst_height)
{
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (src_bgra == nullptr || dst_bgra == nullptr ||
        src_width <= 0 || src_height <= 0 ||
        dst_width <= 0 || dst_height <= 0 ||
        src_stride < src_width * 4 || dst_stride < dst_width * 4) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }

    const int result = libyuv::ARGBScale(
        src_bgra, src_stride, src_width, src_height,
        dst_bgra, dst_stride, dst_width, dst_height,
        libyuv::kFilterBilinear);
    return result == 0 ? OMNI_WEBRTC_MEDIA_STATUS_OK
                       : OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
#else
    (void)src_bgra; (void)src_stride; (void)src_width; (void)src_height;
    (void)dst_bgra; (void)dst_stride; (void)dst_width; (void)dst_height;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_scale_nv12(
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
    int32_t dst_height)
{
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (src_y == nullptr || src_uv == nullptr || dst_y == nullptr || dst_uv == nullptr ||
        src_width <= 0 || src_height <= 0 ||
        dst_width <= 0 || dst_height <= 0 ||
        src_y_stride < src_width || dst_y_stride < dst_width ||
        src_uv_stride < (src_width + (src_width & 1)) ||
        dst_uv_stride < (dst_width + (dst_width & 1)) ||
        (dst_width & 1) != 0 || (dst_height & 1) != 0) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }

    const int result = libyuv::NV12Scale(
        src_y, src_y_stride, src_uv, src_uv_stride, src_width, src_height,
        dst_y, dst_y_stride, dst_uv, dst_uv_stride, dst_width, dst_height,
        libyuv::kFilterBilinear);
    return result == 0 ? OMNI_WEBRTC_MEDIA_STATUS_OK
                       : OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
#else
    (void)src_y; (void)src_y_stride; (void)src_uv; (void)src_uv_stride;
    (void)src_width; (void)src_height;
    (void)dst_y; (void)dst_y_stride; (void)dst_uv; (void)dst_uv_stride;
    (void)dst_width; (void)dst_height;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_nv12_to_bgra(
    const uint8_t* src_y,
    int32_t src_y_stride,
    const uint8_t* src_uv,
    int32_t src_uv_stride,
    uint8_t* dst_bgra,
    int32_t dst_stride,
    int32_t width,
    int32_t height)
{
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (src_y == nullptr || src_uv == nullptr || dst_bgra == nullptr ||
        width <= 0 || height <= 0 ||
        src_y_stride < width || src_uv_stride < (width + (width & 1)) ||
        dst_stride < width * 4) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }

    // libyuv "ARGB" on little-endian == byte order B,G,R,A == our BGRA.
    const int result = libyuv::NV12ToARGB(
        src_y, src_y_stride, src_uv, src_uv_stride,
        dst_bgra, dst_stride, width, height);
    return result == 0 ? OMNI_WEBRTC_MEDIA_STATUS_OK
                       : OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
#else
    (void)src_y; (void)src_y_stride; (void)src_uv; (void)src_uv_stride;
    (void)dst_bgra; (void)dst_stride; (void)width; (void)height;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

// Audio Phase 2a-step3 (2026-05-21): in-process smoke shim that exercises
// OmniAudioSender end-to-end. Constructs a sender at 32 kbps, pushes one
// 20 ms frame of silent PCM (mono, 48 kHz, 960 samples), drains the
// outgoing RTP queue and validates the very first packet's header. The
// validation is deliberately strict on the layout we control (V=2, PT,
// SSRC) and lenient on opus-specific bytes (those are tested by the
// pure libopus smoke in step 1). Used by the smoke runner; not part of
// the production C# surface (the real send path is exposed in step 4).
//
namespace {
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
// Validates that every collected packet is an audio RTP packet: version=2,
// PT=kDefaultPayloadType, SSRC=kAudioSsrc. False on any wire-shape violation.
bool ValidateAudioRtpPacketsForSmoke(const std::vector<std::vector<uint8_t>>& packets)
{
    for (const std::vector<uint8_t>& rtp : packets) {
        if (rtp.size() < 12) {
            return false;
        }
        const uint8_t version = static_cast<uint8_t>((rtp[0] >> 6) & 0x03);
        const uint8_t payload_type = static_cast<uint8_t>(rtp[1] & 0x7F);
        const uint32_t ssrc =
            (static_cast<uint32_t>(rtp[8]) << 24) |
            (static_cast<uint32_t>(rtp[9]) << 16) |
            (static_cast<uint32_t>(rtp[10]) << 8) |
            static_cast<uint32_t>(rtp[11]);
        if (version != 2 ||
            payload_type != OmniAudioSender::kDefaultPayloadType ||
            ssrc != OmniAudioSender::kAudioSsrc) {
            return false;
        }
    }
    return true;
}

// Collects what an OmniAudioSender emits. The capture smoke writes from the WASAPI worker, so
// the collector is locked.
struct AudioPacketCollector {
    std::mutex mutex;
    std::vector<std::vector<uint8_t>> packets;

    OmniAudioSender::PacketOut Output()
    {
        return [this](const uint8_t* data, size_t size) {
            std::lock_guard<std::mutex> lock(mutex);
            packets.emplace_back(data, data + size);
        };
    }
};
#endif
} // namespace

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_smoke_audio_roundtrip(void)
{
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    std::string create_error;
    auto sender = OmniAudioSender::Create(32000, &create_error);
    if (!sender) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }

    AudioPacketCollector collector;
    sender->SetPacketOut(collector.Output());
    std::array<int16_t, OmniAudioSender::kFrameSamples> silence{};
    sender->PushPcm(silence.data(), silence.size());

    if (collector.packets.empty() || !ValidateAudioRtpPacketsForSmoke(collector.packets)) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

// Task #84 (2026-06-06): HW H.264 decode (MF/DXVA, D3D11 mode) golden smoke. Encodes NV12
// color-bars with the MF HW encoder, decodes them with MediaFoundationH264Decoder into an NV12
// D3D11 texture (the zero-copy target for the renderer), reads back the Y plane and asserts the
// color-bar luma pattern survived (middle > left > right). NOT_IMPLEMENTED (skip) when the
// machine has no HW H.264 encoder/decoder MFT — such a machine does not offer H.264 at all (no
// software H.264 in the build; AV1 in software instead). See docs/notes/hw-decode-CONTINUE.md.
extern "C" OmniWebRtcMediaStatus omni_webrtc_media_smoke_mf_h264_hw_decode(void)
{
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    constexpr int kWidth = 320;
    constexpr int kHeight = 192;
    constexpr int kFps = 30;

    // D3D11 device with VIDEO_SUPPORT + multithread protection (required for MF D3D11 decode).
    Microsoft::WRL::ComPtr<ID3D11Device> device;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context;
    const UINT flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
    D3D_FEATURE_LEVEL feature_level = D3D_FEATURE_LEVEL_11_1;
    HRESULT hr = ::D3D11CreateDevice(
        nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, flags, nullptr, 0, D3D11_SDK_VERSION,
        device.GetAddressOf(), &feature_level, context.GetAddressOf());
    if (FAILED(hr) || !device) {
        return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;  // no HW device w/ video support → skip
    }
    Microsoft::WRL::ComPtr<ID3D10Multithread> multithread;
    if (SUCCEEDED(device.As(&multithread)) && multithread) {
        multithread->SetMultithreadProtected(TRUE);
    }

    // NV12 color-bars: Y = 76/149/29 across thirds, UV neutral (128).
    std::vector<uint8_t> y_plane(static_cast<size_t>(kWidth) * static_cast<size_t>(kHeight));
    std::vector<uint8_t> uv_plane(static_cast<size_t>(kWidth) * static_cast<size_t>((kHeight + 1) / 2), 128);
    for (int row = 0; row < kHeight; ++row) {
        uint8_t* r = y_plane.data() + static_cast<size_t>(row) * static_cast<size_t>(kWidth);
        for (int x = 0; x < kWidth; ++x) {
            r[x] = x < kWidth / 3 ? 76 : (x < (kWidth * 2 / 3) ? 149 : 29);
        }
    }

    std::string error;
    auto encoder = MediaFoundationH264Encoder::Create(kWidth, kHeight, kFps, 1'000'000, error);
    if (!encoder) {
        return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;  // no HW encoder → can't run this smoke
    }
    auto decoder = MediaFoundationH264Decoder::Create(device.Get(), kWidth, kHeight, error);
    if (!decoder) {
        return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;  // no HW decoder → skip (SW fallback covers it)
    }

    Microsoft::WRL::ComPtr<ID3D11Texture2D> nv12;
    UINT subresource = 0;
    bool produced = false;
    for (int frame = 0; frame < 24 && !produced; ++frame) {
        EncodedFrame encoded;
        const OmniWebRtcMediaStatus enc_status = encoder->EncodeNv12(
            y_plane.data(), kWidth, uv_plane.data(), kWidth,
            static_cast<int64_t>(frame) * 33'000, frame == 0, static_cast<uint32_t>(frame), encoded);
        if (enc_status != OMNI_WEBRTC_MEDIA_STATUS_OK || encoded.data.empty()) {
            continue;  // encoder async latency — output not ready this frame
        }
        const OmniWebRtcMediaStatus dec_status = decoder->Decode(
            encoded.data.data(), encoded.data.size(),
            static_cast<int64_t>(frame) * 330'000, nv12, subresource, produced);
        if (dec_status != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }
    }
    if (!produced || !nv12) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;  // HW decoder present but produced no frame
    }

    // Readback the NV12 Y plane via a staging copy; assert the color-bar luma pattern survived.
    D3D11_TEXTURE2D_DESC desc{};
    nv12->GetDesc(&desc);
    D3D11_TEXTURE2D_DESC staging_desc = desc;
    staging_desc.Width = static_cast<UINT>(kWidth);
    staging_desc.Height = static_cast<UINT>(kHeight);
    staging_desc.ArraySize = 1;
    staging_desc.MipLevels = 1;
    staging_desc.Usage = D3D11_USAGE_STAGING;
    staging_desc.BindFlags = 0;
    staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    staging_desc.MiscFlags = 0;
    Microsoft::WRL::ComPtr<ID3D11Texture2D> staging;
    if (FAILED(device->CreateTexture2D(&staging_desc, nullptr, &staging))) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }
    context->CopySubresourceRegion(staging.Get(), 0, 0, 0, 0, nv12.Get(), subresource, nullptr);

    D3D11_MAPPED_SUBRESOURCE mapped{};
    if (FAILED(context->Map(staging.Get(), 0, D3D11_MAP_READ, 0, &mapped))) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }
    const auto average_luma = [&](int x0, int x1) -> double {
        long long sum = 0;
        long long count = 0;
        const uint8_t* base = static_cast<const uint8_t*>(mapped.pData);
        for (int row = 0; row < kHeight; ++row) {
            const uint8_t* r = base + static_cast<size_t>(row) * static_cast<size_t>(mapped.RowPitch);
            for (int x = x0; x < x1; ++x) {
                sum += r[x];
                ++count;
            }
        }
        return count == 0 ? 0.0 : static_cast<double>(sum) / static_cast<double>(count);
    };
    const double left = average_luma(kWidth / 12, kWidth / 4);
    const double middle = average_luma(kWidth * 5 / 12, kWidth * 7 / 12);
    const double right = average_luma(kWidth * 3 / 4, kWidth * 11 / 12);
    context->Unmap(staging.Get(), 0);

    if (!(middle > left + 20.0 && left > right + 20.0)) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;  // decoded frame is not the color-bars
    }
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

// Task #84 increment 2 (2026-06-06): GPU NV12->RGB matches libyuv (golden, headless). Renders
// several uniform-color NV12 frames (an NV12 D3D11 texture — the HW decoder's output format)
// through the BT.601 NV12 pixel shader to an offscreen target and asserts each pixel within
// tolerance of libyuv::NV12ToARGB. Uniform frames validate the conversion MATH (matrix / byte
// order / Y+UV plane SRVs) headlessly. Needs libyuv (libwebrtc build).
extern "C" OmniWebRtcMediaStatus omni_webrtc_media_smoke_d3d11_nv12_matches_libyuv(
    char* detail,
    size_t detail_len)
{
    const auto report = [detail, detail_len](const char* what) {
        if (detail != nullptr && detail_len > 0) {
            std::snprintf(detail, detail_len, "%s", what);
        }
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    };
    if (detail != nullptr && detail_len > 0) {
        detail[0] = 0;
    }
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    omni_media::D3D11VideoRenderer renderer;
    if (!renderer.Initialize() || renderer.device() == nullptr) {
        return report("d3d11 device initialize failed");
    }

    constexpr int kWidth = 64;
    constexpr int kHeight = 32;
    constexpr int kTolerance = 16;
    const uint8_t cases[][3] = {
        {81, 90, 240}, {145, 54, 34}, {41, 240, 110}, {128, 128, 128}, {16, 128, 128}, {235, 128, 128},
    };
    int case_index = -1;

    for (const auto& yuv : cases) {
        ++case_index;
        const size_t y_size = static_cast<size_t>(kWidth) * static_cast<size_t>(kHeight);
        const size_t uv_size = static_cast<size_t>(kWidth) * static_cast<size_t>(kHeight / 2);
        std::vector<uint8_t> nv12(y_size + uv_size);
        std::fill(nv12.begin(), nv12.begin() + static_cast<ptrdiff_t>(y_size), yuv[0]);
        uint8_t* uv = nv12.data() + y_size;
        for (size_t i = 0; i + 1 < uv_size; i += 2) {
            uv[i] = yuv[1];
            uv[i + 1] = yuv[2];
        }

        D3D11_TEXTURE2D_DESC desc = {};
        desc.Width = static_cast<UINT>(kWidth);
        desc.Height = static_cast<UINT>(kHeight);
        desc.MipLevels = 1;
        desc.ArraySize = 1;
        desc.Format = DXGI_FORMAT_NV12;
        desc.SampleDesc.Count = 1;
        desc.Usage = D3D11_USAGE_DEFAULT;
        desc.BindFlags = D3D11_BIND_SHADER_RESOURCE;
        D3D11_SUBRESOURCE_DATA init = {};
        init.pSysMem = nv12.data();
        init.SysMemPitch = static_cast<UINT>(kWidth);
        Microsoft::WRL::ComPtr<ID3D11Texture2D> texture;
        if (FAILED(renderer.device()->CreateTexture2D(&desc, &init, texture.GetAddressOf()))) {
            char message[96] = {};
            std::snprintf(message, sizeof(message), "nv12 texture create failed, case %d", case_index);
            return report(message);
        }

        std::vector<uint8_t> gpu(y_size * 4u);
        std::vector<uint8_t> cpu(y_size * 4u);
        // МАРКЕР ДО ЧТЕНИЯ — РАЗДЕЛИТЕЛЬ ДВУХ РАЗНЫХ ОТКАЗОВ.
        //
        // Замер 2026-08-23 показал `gpu-zero 8192/8192`: буфер вернулся полностью нулевым. Но у
        // этого два совершенно разных объяснения, и лечатся они в разных местах: либо GPU
        // действительно нарисовал чёрное, либо буфер вообще никто не переписал, а нули в нём были
        // изначально (vector<uint8_t> зануляется при создании). Заполняем заметным значением: если
        // после чтения маркер уцелел — чтение не состоялось; если стало нулями — чёрное пришло с
        // GPU.
        std::fill(gpu.begin(), gpu.end(), static_cast<uint8_t>(0xAB));
        if (!renderer.RenderNv12ToRgba(texture.Get(), 0, kWidth, kHeight, gpu.data(), gpu.size())) {
            char message[96] = {};
            std::snprintf(message, sizeof(message), "nv12 render failed, case %d", case_index);
            return report(message);
        }
        if (libyuv::NV12ToARGB(
                nv12.data(), kWidth, nv12.data() + y_size, kWidth,
                cpu.data(), kWidth * 4, kWidth, kHeight) != 0) {
            return report("libyuv NV12ToARGB failed");
        }
        // ДИАГНОСТИКА ОТЛИЧАЕТ «НЕ НАРИСОВАЛОСЬ» ОТ «НАРИСОВАЛОСЬ НЕВЕРНО».
        //
        // Флейк #45 давал «pixel mismatch, case 0 byte 2 gpu 0 cpu 254» — и по одному байту нельзя
        // сказать, вернул ли GPU чёрный буфер целиком (то есть рисование не состоялось) или
        // ошибся в цвете. Разница принципиальная: первое — про синхронизацию и состояние
        // конвейера, второе — про шейдер и коэффициенты. Считаем сводку по всему буферу и
        // печатаем её вместе с первым несовпадением, чтобы следующее срабатывание сразу
        // отвечало на этот вопрос.
        size_t mismatched = 0;
        size_t gpu_zero_bytes = 0;
        size_t marker_survivors = 0;
        size_t first_bad = gpu.size();
        for (size_t i = 0; i < gpu.size(); ++i) {
            if (gpu[i] == 0) {
                ++gpu_zero_bytes;
            }
            if (gpu[i] == 0xAB) {
                ++marker_survivors;
            }
            const int diff = static_cast<int>(gpu[i]) - static_cast<int>(cpu[i]);
            if (diff > kTolerance || diff < -kTolerance) {
                ++mismatched;
                if (first_bad == gpu.size()) {
                    first_bad = i;
                }
            }
        }
        if (mismatched > 0) {
            // Потеря устройства (TDR) и настоящее расхождение пикселей — РАЗНЫЕ события, и путать
            // их нельзя: первое приходит извне (GPU делят композитор, оверлеи, живые обои), второе
            // означает дефект конверсии. Префикс "device-lost:" даёт вызывающему их различить.
            const HRESULT removed = renderer.device() != nullptr
                ? renderer.device()->GetDeviceRemovedReason()
                : S_OK;
            // Префикс приписывается ОТДЕЛЬНЫМ шагом, а не тернарником внутри формата: иначе
            // внешний формат съел бы только строку, а проценты внутреннего остались бы
            // неразвёрнутыми.
            char body[256] = {};
            std::snprintf(
                body, sizeof(body),
                "nv12 pixel mismatch, case %d byte %zu gpu %d cpu %d; mismatched %zu/%zu, "
                "gpu-zero %zu/%zu, marker-alive %zu/%zu, removedReason 0x%08lX",
                case_index, first_bad, static_cast<int>(gpu[first_bad]),
                static_cast<int>(cpu[first_bad]), mismatched, gpu.size(),
                gpu_zero_bytes, gpu.size(), marker_survivors, gpu.size(),
                static_cast<unsigned long>(removed));
            char message[288] = {};
            std::snprintf(
                message, sizeof(message), "%s%s",
                removed != S_OK ? "device-lost: " : "", body);
            return report(message);
        }
    }
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    (void)detail;
    (void)detail_len;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

// #5 increment 1a (2026-06-01): D3D11 offscreen render-target + staging readback.
// Headless GPU plumbing for the SwapChainPanel video renderer
// (docs/notes/video-gpu-render-swapchainpanel.md). Clears a small BGRA target to a
// known color and reads it back, asserting the round-trip — proves device + RTV +
// staging map without a display. NOT_IMPLEMENTED off-Windows.
extern "C" OmniWebRtcMediaStatus omni_webrtc_media_smoke_d3d11_solid_readback(void)
{
#if defined(_WIN32)
    omni_media::D3D11VideoRenderer renderer;
    if (!renderer.Initialize()) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }

    constexpr int kWidth = 16;
    constexpr int kHeight = 8;
    constexpr uint8_t kR = 10;
    constexpr uint8_t kG = 20;
    constexpr uint8_t kB = 30;
    uint8_t pixels[kWidth * kHeight * 4] = {};
    if (!renderer.RenderSolidAndReadback(kR, kG, kB, kWidth, kHeight, pixels, sizeof(pixels))) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }

    for (int i = 0; i < kWidth * kHeight; ++i) {
        const uint8_t* px = pixels + (static_cast<size_t>(i) * 4u);
        const int db = px[0] >= kB ? px[0] - kB : kB - px[0];
        const int dg = px[1] >= kG ? px[1] - kG : kG - px[1];
        const int dr = px[2] >= kR ? px[2] - kR : kR - px[2];
        if (db > 1 || dg > 1 || dr > 1 || px[3] != 255) {
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }
    }
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

// ВИРТУАЛЬНЫЕ ЧАСЫ ДЛЯ СТЕНДОВ (#59). В продукте не зовутся никем.
//
// Под сборкой-заглушкой (без libwebrtc) их нет: GlobalSimulatedTimeController — часть апстрима, а
// заглушка собирается именно без него. Отсутствие охраны здесь роняло сборку-заглушку целиком:
// три функции ссылались на g_simulated_time, объявленный внутри libwebrtc-ветки, и компилятор
// терял на них весь конец файла.
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_test_simulated_time_begin(void)
{
    std::lock_guard<std::mutex> lock(g_simulated_time_mutex);
    if (g_simulated_time) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
    // Начальная метка ненулевая: часть кода апстрима считает нулевое время «не выставлено».
    g_simulated_time = std::make_unique<webrtc::GlobalSimulatedTimeController>(
        webrtc::Timestamp::Seconds(1000));
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_test_simulated_time_advance_ms(int64_t duration_ms)
{
    if (duration_ms < 0) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
    webrtc::GlobalSimulatedTimeController* controller = nullptr;
    {
        std::lock_guard<std::mutex> lock(g_simulated_time_mutex);
        controller = g_simulated_time.get();
    }
    if (controller == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
    // Замок отпущен намеренно: во время advance выполняются задачи очередей, а они могут дойти
    // до кода, который снова спросит окружение. Держать замок здесь значило бы заблокировать
    // самого себя.
    controller->AdvanceTime(webrtc::TimeDelta::Millis(duration_ms));
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_test_simulated_time_end(void)
{
    std::lock_guard<std::mutex> lock(g_simulated_time_mutex);
    if (!g_simulated_time) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
    g_simulated_time.reset();
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
}

#else

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_test_simulated_time_begin(void)
{
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_test_simulated_time_advance_ms(int64_t duration_ms)
{
    (void)duration_ms;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_test_simulated_time_end(void)
{
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
}

#endif

// #5 increment 1b (2026-06-01): GPU I420->RGB conversion matches libyuv (golden). Renders
// several uniform-color I420 frames through the BT.601 pixel shader to an offscreen target,
// reads them back, and asserts each pixel is within tolerance of libyuv::I420ToARGB. Uniform
// frames sidestep chroma-upsampling differences so this validates the conversion MATH
// (catches wrong matrix / byte order / inversion) headlessly. Needs libyuv (libwebrtc build).

OmniWebRtcMediaStatus omni_webrtc_media_smoke_d3d11_i420_matches_libyuv(
    char* detail,
    size_t detail_len)
{
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    const auto report = [&](const char* what) {
        if (detail != nullptr && detail_len > 0) {
            std::snprintf(detail, detail_len, "%s", what);
        }
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    };
    omni_media::D3D11VideoRenderer renderer;
    if (!renderer.Initialize()) {
        return report("i420: renderer init failed");
    }

    constexpr int kWidth = 64;
    constexpr int kHeight = 32;
    constexpr int kChromaWidth = (kWidth + 1) / 2;
    constexpr int kChromaHeight = (kHeight + 1) / 2;
    constexpr int kTolerance = 16;
    const uint8_t cases[][3] = {
        {81, 90, 240}, {145, 54, 34}, {41, 240, 110}, {128, 128, 128}, {16, 128, 128}, {235, 128, 128},
    };

    int case_index = -1;

    for (const auto& yuv : cases) {
        ++case_index;
        std::vector<uint8_t> y_plane(static_cast<size_t>(kWidth) * static_cast<size_t>(kHeight), yuv[0]);
        std::vector<uint8_t> u_plane(static_cast<size_t>(kChromaWidth) * static_cast<size_t>(kChromaHeight), yuv[1]);
        std::vector<uint8_t> v_plane(static_cast<size_t>(kChromaWidth) * static_cast<size_t>(kChromaHeight), yuv[2]);
        std::vector<uint8_t> gpu(static_cast<size_t>(kWidth) * static_cast<size_t>(kHeight) * 4u);
        std::vector<uint8_t> cpu(static_cast<size_t>(kWidth) * static_cast<size_t>(kHeight) * 4u);

        // МАРКЕР ДО ЧТЕНИЯ — РАЗДЕЛИТЕЛЬ ДВУХ РАЗНЫХ ОТКАЗОВ.
        //
        // Замер 2026-08-23 показал `gpu-zero 8192/8192`: буфер вернулся полностью нулевым. Но у
        // этого два совершенно разных объяснения, и лечатся они в разных местах: либо GPU
        // действительно нарисовал чёрное, либо буфер вообще никто не переписал, а нули в нём были
        // изначально (vector<uint8_t> зануляется при создании). Заполняем заметным значением: если
        // после чтения маркер уцелел — чтение не состоялось; если стало нулями — чёрное пришло с
        // GPU.
        std::fill(gpu.begin(), gpu.end(), static_cast<uint8_t>(0xAB));
        if (!renderer.RenderI420ToRgba(
                y_plane.data(), kWidth, u_plane.data(), kChromaWidth, v_plane.data(), kChromaWidth,
                kWidth, kHeight, gpu.data(), gpu.size())) {
            char message[96] = {};
            std::snprintf(message, sizeof(message), "i420 render failed, case %d", case_index);
            return report(message);
        }
        if (libyuv::I420ToARGB(
                y_plane.data(), kWidth, u_plane.data(), kChromaWidth, v_plane.data(), kChromaWidth,
                cpu.data(), kWidth * 4, kWidth, kHeight) != 0) {
            return report("libyuv I420ToARGB failed");
        }
        // Та же сводка, что у NV12: она отличает «рисование не состоялось» (буфер чёрный целиком)
        // от «нарисовано неверно» (искажены цвета). Флейк 2026-08-23 сработал в I420, а объяснять
        // себя умел только NV12 — эта асимметрия и стоила лишнего прогона.
        size_t mismatched = 0;
        size_t gpu_zero_bytes = 0;
        size_t marker_survivors = 0;
        size_t first_bad = gpu.size();
        for (size_t i = 0; i < gpu.size(); ++i) {
            if (gpu[i] == 0) {
                ++gpu_zero_bytes;
            }
            if (gpu[i] == 0xAB) {
                ++marker_survivors;
            }
            const int diff = static_cast<int>(gpu[i]) - static_cast<int>(cpu[i]);
            if (diff > kTolerance || diff < -kTolerance) {
                ++mismatched;
                if (first_bad == gpu.size()) {
                    first_bad = i;
                }
            }
        }
        if (mismatched > 0) {
            // Потеря устройства (TDR) и настоящее расхождение пикселей — РАЗНЫЕ события, и путать
            // их нельзя: первое приходит извне (GPU делят композитор, оверлеи, живые обои), второе
            // означает дефект конверсии. Префикс "device-lost:" даёт вызывающему их различить.
            const HRESULT removed = renderer.device() != nullptr
                ? renderer.device()->GetDeviceRemovedReason()
                : S_OK;
            // Префикс приписывается ОТДЕЛЬНЫМ шагом, а не тернарником внутри формата: иначе
            // внешний формат съел бы только строку, а проценты внутреннего остались бы
            // неразвёрнутыми.
            char body[256] = {};
            std::snprintf(
                body, sizeof(body),
                "i420 pixel mismatch, case %d byte %zu gpu %d cpu %d; mismatched %zu/%zu, "
                "gpu-zero %zu/%zu, marker-alive %zu/%zu, removedReason 0x%08lX",
                case_index, first_bad, static_cast<int>(gpu[first_bad]),
                static_cast<int>(cpu[first_bad]), mismatched, gpu.size(),
                gpu_zero_bytes, gpu.size(), marker_survivors, gpu.size(),
                static_cast<unsigned long>(removed));
            char message[288] = {};
            std::snprintf(
                message, sizeof(message), "%s%s",
                removed != S_OK ? "device-lost: " : "", body);
            return report(message);
        }
    }
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    (void)detail;
    (void)detail_len;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

// #5 increment 2 (2026-06-01): composition-swapchain render path (headless-verifiable). Creates
// a CreateSwapChainForComposition swapchain, renders a uniform I420 frame to its backbuffer,
// reads it back (no Present) and asserts vs libyuv. Swapchain-create failure is tolerated
// (some headless/locked GPUs); the present-to-panel step is validated live.
extern "C" OmniWebRtcMediaStatus omni_webrtc_media_smoke_d3d11_swapchain_i420(void)
{
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    omni_media::D3D11VideoRenderer renderer;
    if (!renderer.Initialize()) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }

    constexpr int kWidth = 64;
    constexpr int kHeight = 32;
    constexpr int kChromaWidth = (kWidth + 1) / 2;
    constexpr int kChromaHeight = (kHeight + 1) / 2;
    Microsoft::WRL::ComPtr<IDXGISwapChain1> swap_chain;
    if (!renderer.CreateCompositionSwapChain(kWidth, kHeight, swap_chain.GetAddressOf())) {
        // Composition swapchain unavailable (headless / locked GPU). The conversion math is
        // already covered by omni_webrtc_media_smoke_d3d11_i420_matches_libyuv.
        return OMNI_WEBRTC_MEDIA_STATUS_OK;
    }

    std::vector<uint8_t> y_plane(static_cast<size_t>(kWidth) * static_cast<size_t>(kHeight), 145);
    std::vector<uint8_t> u_plane(static_cast<size_t>(kChromaWidth) * static_cast<size_t>(kChromaHeight), 54);
    std::vector<uint8_t> v_plane(static_cast<size_t>(kChromaWidth) * static_cast<size_t>(kChromaHeight), 34);
    std::vector<uint8_t> gpu(static_cast<size_t>(kWidth) * static_cast<size_t>(kHeight) * 4u);
    std::vector<uint8_t> cpu(static_cast<size_t>(kWidth) * static_cast<size_t>(kHeight) * 4u);

    if (!renderer.RenderI420ToSwapChainAndReadback(
            y_plane.data(), kWidth, u_plane.data(), kChromaWidth, v_plane.data(), kChromaWidth,
            kWidth, kHeight, gpu.data(), gpu.size())) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }
    if (libyuv::I420ToARGB(
            y_plane.data(), kWidth, u_plane.data(), kChromaWidth, v_plane.data(), kChromaWidth,
            cpu.data(), kWidth * 4, kWidth, kHeight) != 0) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }
    for (size_t i = 0; i < gpu.size(); ++i) {
        const int diff = static_cast<int>(gpu[i]) - static_cast<int>(cpu[i]);
        if (diff > 16 || diff < -16) {
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }
    }

    // A window of another shape: the 64x32 frame must sit whole in a 64x64 backbuffer — a band of
    // picture across the middle (rows 16..47), black bars above and below — not stretched over all
    // of it (FitViewport, 2026-10-05). The same-shape pass above is the negative half: it fills.
    if (!renderer.ResizeSwapChain(kWidth, kWidth)) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }
    std::vector<uint8_t> square(static_cast<size_t>(kWidth) * static_cast<size_t>(kWidth) * 4u);
    if (!renderer.RenderI420ToSwapChainAndReadback(
            y_plane.data(), kWidth, u_plane.data(), kChromaWidth, v_plane.data(), kChromaWidth,
            kWidth, kHeight, square.data(), square.size())) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }
    const auto pixel_matches = [&](int row, int column, const uint8_t* expected) {
        const uint8_t* got = square.data() + (static_cast<size_t>(row) * kWidth + column) * 4u;
        for (int c = 0; c < 3; ++c) {
            const int diff = static_cast<int>(got[c]) - static_cast<int>(expected[c]);
            if (diff > 16 || diff < -16) {
                return false;
            }
        }
        return true;
    };
    const uint8_t black[3] = {0, 0, 0};
    for (int column = 0; column < kWidth; column += 7) {
        if (!pixel_matches(2, column, black) || !pixel_matches(kWidth - 3, column, black) ||
            !pixel_matches(kWidth / 2, column, cpu.data())) {
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }
    }
    renderer.DetachSwapChain();
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

// Audio Phase 2a-step3 (2026-05-21): WASAPI → OmniAudioSender wiring smoke.
// Constructs OmniWasapiCapture + OmniAudioSender, binds the capture
// FrameCallback to PushPcm so the worker thread feeds the sender as
// audio arrives. Then Start, sleep ~80 ms (gives the worker enough wall
// time to observe 3-4 packets at 20 ms cadence if a real mic exists),
// Stop, then drain.
//
// Headless-safe: on machines without a usable capture endpoint the
// worker observes zero packet-frames per pull and the callback is never
// invoked, so the drain returns 0 packets. We accept that — the smoke
// proves Start/Stop is safe and the contract types line up. If any
// packets DID arrive they must validate as audio RTP (version=2, PT, SSRC).
extern "C" OmniWebRtcMediaStatus omni_webrtc_media_smoke_audio_capture_to_sender(void)
{
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    std::string create_error;
    auto sender = OmniAudioSender::Create(32000, &create_error);
    if (!sender) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }

    OmniAudioSender* sender_ptr = sender.get();
    AudioPacketCollector collector;
    sender->SetPacketOut(collector.Output());
    OmniWasapiCapture capture;
    HRESULT hr = capture.Start([sender_ptr](const int16_t* samples, size_t sample_count) {
        sender_ptr->PushPcm(samples, sample_count);
    });
    // Start may legitimately fail on headless CI / containers (e.g.
    // RPC_E_NO_GOOD_SECURITY_PACKAGES or ERROR_NOT_FOUND when there
    // is no audio session subsystem). Treat any failure as "no capture
    // available" — the contract is still validated below by sender state.
    const bool start_ok = SUCCEEDED(hr);

    if (start_ok) {
        std::this_thread::sleep_for(std::chrono::milliseconds(80));
        capture.Stop();
    }

    std::vector<std::vector<uint8_t>> packets;
    {
        std::lock_guard<std::mutex> lock(collector.mutex);
        packets = collector.packets;
    }
    if (!packets.empty()) {
        if (!ValidateAudioRtpPacketsForSmoke(packets)) {
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }
    } else if (start_ok) {
        // Start succeeded — the worker ran for ~80 ms. Either the
        // device produced no data (headless / muted mic) or the
        // format diverged from the 48 kHz mono PCM16 fast path
        // (skeleton policy: drop). Both are valid for the smoke.
        // Nothing further to check.
    }

    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

// Sender → NetEq receiver roundtrip: 200 ms of a tone through the production Opus sender, into the
// receiver, pulled back in 10 ms blocks. The tone must come out (a decoded silence would prove
// nothing), and — the negative half — a video packet must not be taken as audio.
extern "C" OmniWebRtcMediaStatus omni_webrtc_media_smoke_audio_sender_to_receiver(void)
{
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    std::string create_error;
    auto sender = OmniAudioSender::Create(32000, &create_error);
    auto receiver = OmniNetEqAudioReceiver::Create(&create_error);
    if (!sender || !receiver) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }

    AudioPacketCollector collector;
    sender->SetPacketOut(collector.Output());
    std::array<int16_t, OmniAudioSender::kFrameSamples> tone{};
    for (int frame = 0; frame < 10; ++frame) {
        for (size_t i = 0; i < tone.size(); ++i) {
            const double t = static_cast<double>(frame * OmniAudioSender::kFrameSamples + static_cast<int>(i)) / 48000.0;
            tone[i] = static_cast<int16_t>(8000.0 * std::sin(2.0 * 3.14159265358979 * 440.0 * t));
        }
        sender->PushPcm(tone.data(), tone.size());
    }
    if (collector.packets.size() != 10) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }
    for (const std::vector<uint8_t>& packet : collector.packets) {
        if (receiver->InsertRtpPacket(packet.data(), packet.size()) !=
            OmniNetEqAudioReceiver::InsertResult::kAccepted) {
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }
    }
    std::array<uint8_t, 16> video{};
    video[0] = 0x80u;
    video[1] = 102u;
    if (receiver->InsertRtpPacket(video.data(), video.size()) !=
        OmniNetEqAudioReceiver::InsertResult::kNotAudio) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }

    int64_t energy = 0;
    std::array<int16_t, OmniNetEqAudioReceiver::kPullSamples> block{};
    for (int pull = 0; pull < 20; ++pull) {
        receiver->Pull10Ms(block.data());
        for (const int16_t sample : block) {
            energy += std::abs(static_cast<int32_t>(sample));
        }
    }
    return energy > 0 ? OMNI_WEBRTC_MEDIA_STATUS_OK : OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
#else
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
namespace {
// Сценарий джиттера: что пришло и когда. 20 мс на кадр, базовая задержка пути 40 мс; каждый пятый
// пакет меняется местами с соседом (как ~20 % перестановок на Wi-Fi-аплинке ноутбука), кадры
// 150..164 задержаны затором и приходят одной пачкой через 300 мс.
struct AudioScenarioPacket {
    int64_t arrival_ms = 0;
    std::vector<uint8_t> rtp;
};

std::vector<AudioScenarioPacket> BuildAudioJitterScenario(int frames, bool reorder)
{
    std::vector<std::vector<uint8_t>> packets;
    std::string error;
    auto sender = OmniAudioSender::Create(32000, &error);
    if (!sender) {
        return {};
    }
    sender->SetPacketOut([&packets](const uint8_t* data, size_t size) {
        packets.emplace_back(data, data + size);
    });
    std::vector<int16_t> pcm(OmniAudioSender::kFrameSamples);
    for (int frame = 0; frame < frames; ++frame) {
        for (int i = 0; i < OmniAudioSender::kFrameSamples; ++i) {
            const double t = static_cast<double>(frame * OmniAudioSender::kFrameSamples + i) / 48000.0;
            pcm[static_cast<size_t>(i)] = static_cast<int16_t>(8000.0 * std::sin(2.0 * 3.14159265358979 * 440.0 * t));
        }
        sender->PushPcm(pcm.data(), pcm.size());
    }
    std::vector<AudioScenarioPacket> out;
    for (size_t i = 0; i < packets.size(); ++i) {
        int64_t arrival = static_cast<int64_t>(i) * 20 + 40;
        if (i >= 150 && i < 165) {
            arrival = 165 * 20 + 40;  // затор: пачка разом
        }
        out.push_back(AudioScenarioPacket{arrival, std::move(packets[i])});
    }
    for (size_t i = 0; reorder && i + 1 < out.size(); i += 5) {
        if (i >= 145 && i < 170) {
            continue;
        }
        std::swap(out[i].rtp, out[i + 1].rtp);  // n+1 приходит раньше n
    }
    return out;
}

struct AudioScenarioResult {
    int64_t end_latency_ms = 0;
    int64_t max_latency_ms = 0;
    int64_t extra_audio_ms = 0;
    int64_t silence_ms = 0;
};

// Приёмник на NetEq, на виртуальных часах: пакеты вставляются в момент прихода, звук тянется по
// 10 мс, как тянет устройство. Задержка — то, что держит буфер NetEq.
AudioScenarioResult RunAudioScenarioThroughNetEq(const std::vector<AudioScenarioPacket>& packets, int frames)
{
    AudioScenarioResult result;
    webrtc::SimulatedClock clock(webrtc::Timestamp::Millis(1'000'000));
    std::string error;
    auto receiver = OmniNetEqAudioReceiver::Create(&error, &clock);
    if (!receiver) {
        result.end_latency_ms = -1;
        return result;
    }
    std::array<int16_t, OmniNetEqAudioReceiver::kPullSamples> block{};
    size_t next = 0;
    // До конца потока: после него NetEq честно маскирует тишину, и она не про качество приёма.
    const int64_t end_ms = static_cast<int64_t>(frames) * 20;
    for (int64_t now = 0; now <= end_ms; now += 10) {
        while (next < packets.size() && packets[next].arrival_ms <= now) {
            (void)receiver->InsertRtpPacket(packets[next].rtp.data(), packets[next].rtp.size());
            ++next;
        }
        receiver->Pull10Ms(block.data());
        clock.AdvanceTime(webrtc::TimeDelta::Millis(10));
        const int64_t latency = receiver->CurrentStatistics().current_buffer_size_ms;
        if (now >= 200) {
            result.max_latency_ms = std::max(result.max_latency_ms, latency);
        }
        if (now == static_cast<int64_t>(frames) * 20) {
            result.end_latency_ms = latency;
        }
    }
    const webrtc::NetEqLifetimeStatistics lifetime = receiver->LifetimeStatistics();
    result.silence_ms = static_cast<int64_t>(lifetime.concealed_samples / 48);
    result.extra_audio_ms = static_cast<int64_t>(lifetime.inserted_samples_for_deceleration / 48) -
                            static_cast<int64_t>(lifetime.removed_samples_for_acceleration / 48);
    return result;
}
} // namespace
#endif

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_smoke_audio_jitter(char* report, size_t report_len)
{
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    constexpr int kFrames = 500;
    // ПОРОГИ — ЧТО ОБЯЗАН ДЕРЖАТЬ ПРИЁМ (виртуальные часы: прогон детерминирован).
    // Прежний самодельный приёмник на этом же сценарии: задержка к концу 990 мс (в упоре в
    // предохранитель, +1880 мс повторного звука) с перестановками и 310 мс — навсегда — после
    // одного затора. NetEq: 233 и 101 мс. Пороги с запасом над NetEq и далеко под прежним.
    constexpr int64_t kMaxEndLatencyWithReorderMs = 350;
    constexpr int64_t kMaxEndLatencyAfterStallMs = 150;
    constexpr int64_t kMaxConcealedMs = 400;  // затор 300 мс заполнить нечем, кроме маскировки
    bool within_bounds = true;
    std::ostringstream line;
    for (const bool reorder : {true, false}) {
        const std::vector<AudioScenarioPacket> packets = BuildAudioJitterScenario(kFrames, reorder);
        if (packets.size() != static_cast<size_t>(kFrames)) {
            return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
        }
        const AudioScenarioResult neteq = RunAudioScenarioThroughNetEq(packets, kFrames);
        line << (reorder ? "reorder+stall " : "stall-only ")
             << R"({"receiver":"neteq","end_latency_ms":)" << neteq.end_latency_ms
             << R"(,"max_latency_ms":)" << neteq.max_latency_ms
             << R"(,"stretch_ms":)" << neteq.extra_audio_ms
             << R"(,"concealed_ms":)" << neteq.silence_ms << "} ";
        const int64_t max_end = reorder ? kMaxEndLatencyWithReorderMs : kMaxEndLatencyAfterStallMs;
        within_bounds = within_bounds && neteq.end_latency_ms >= 0 &&
                        neteq.end_latency_ms <= max_end && neteq.silence_ms <= kMaxConcealedMs;
    }
    size_t required = 0;
    (void)CopyString(line.str(), report, report_len, &required);
    return within_bounds ? OMNI_WEBRTC_MEDIA_STATUS_OK : OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
#else
    (void)report;
    (void)report_len;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

// Audio Phase 2b-step1 (2026-05-23): OmniWasapiRender lifecycle smoke.
// Start the render endpoint, write a small chunk of PCM (mono 48 kHz
// silence — keeps it audible-safe), sleep ~80 ms to give the worker
// time to drain into the device, then Stop. Headless-safe: if no
// render endpoint exists, Start returns a COM HRESULT and we report
// it via stderr without failing.
extern "C" OmniWebRtcMediaStatus omni_webrtc_media_smoke_audio_render_lifecycle(void)
{
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    OmniWasapiRender render;
    size_t pulled = 0;
    render.SetPcmSource([&pulled](int16_t* dst, size_t sample_count) {
        std::fill(dst, dst + sample_count, int16_t{0});  // silence
        pulled += sample_count;
    });
    HRESULT hr = render.Start();
    if (FAILED(hr)) {
        // Headless / no render endpoint: not a smoke failure.
        return OMNI_WEBRTC_MEDIA_STATUS_OK;
    }
    std::this_thread::sleep_for(std::chrono::milliseconds(80));
    render.Stop();
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

// Audio Phase 2a-step4 (2026-05-23): production FFI surface for the
// audio capture+send pipeline. Three exports mirror the video
// session lifecycle (create / destroy / drain) but skip the rest of
// the libwebrtc machinery — audio doesn't need NACK / pacing /
// adaptation. C# allocates one handle per call, drains RTP packets
// on a timer, and forwards them through the existing
// omni_realtime_media_send_datagram path with the audio PT (111)
// so the router can fan out by payload type.
//
// Lifecycle ownership: the opaque OmniWebRtcMediaAudioCapture* points
// to a heap-allocated struct whose `impl` holds a
// std::unique_ptr-like pair of OmniWasapiCapture + OmniAudioSender
// inside a small internal class. We name the internal class
// `OmniAudioCaptureImpl` to make destruction-order intent legible:
// the impl destructor stops the capture worker BEFORE the sender is
// destroyed, because the worker thread invokes PushPcm via the
// closure.
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
namespace {

// Shared AEC reference (2026-06-05). Acoustic echo cancellation needs the far-end render
// audio (what is played out the speaker) fed to the capture APM as its reverse stream — but
// capture and playback are separate FFI objects and media internals can't cross the FFI.
// webrtc::AudioProcessing is ref-counted, so the active capture session publishes its APM
// here and the playback session borrows a ref (the copy keeps it alive for the duration of a
// reverse-stream call). The "one call per profile" invariant means at most one APM is live.
std::mutex g_echo_apm_mutex;
webrtc::scoped_refptr<webrtc::AudioProcessing> g_echo_apm;

// How many 10 ms far-end blocks the playback side has handed the canceller since the current
// capture session published its APM.
//
// Without this an echo complaint is undiagnosable: AEC3 sitting at zero cancellation looks
// exactly the same whether it is being fed a reference and failing, or never being fed at
// all (playback session started before capture published, capture APM failed to build, the
// user's output device is not the one we render to). A live pass on 2026-08-11 howled and
// there was no way to tell which. Reset on publish so the number always describes THIS call.
std::atomic<uint64_t> g_echo_reverse_blocks{0};

void PublishEchoApm(webrtc::scoped_refptr<webrtc::AudioProcessing> apm)
{
    std::lock_guard<std::mutex> lock(g_echo_apm_mutex);
    g_echo_apm = std::move(apm);
    g_echo_reverse_blocks.store(0, std::memory_order_relaxed);
}

void ClearEchoApm(const webrtc::AudioProcessing* apm)
{
    std::lock_guard<std::mutex> lock(g_echo_apm_mutex);
    if (g_echo_apm.get() == apm) {
        g_echo_apm = nullptr;
    }
}

webrtc::scoped_refptr<webrtc::AudioProcessing> AcquireEchoApm()
{
    std::lock_guard<std::mutex> lock(g_echo_apm_mutex);
    return g_echo_apm;
}

// Voice-activity level for the group-call speaking indicator. Instantaneous RMS of a PCM
// block, normalised to ~0..1, smoothed with fast attack / slow release so a participant tile's
// "speaking" ring lights immediately on speech and fades over ~150 ms instead of flickering
// per 10 ms block. Runs on the audio thread; the UI polls the atomic. Same helper feeds the
// local capture level (self tile) and each remote playback level (per-sender active-speaker).
static inline void OmniUpdateSpeakingLevel(
    std::atomic<float>& level, std::atomic<int64_t>& updated_ms,
    const int16_t* samples, size_t count)
{
    if (samples == nullptr || count == 0) {
        return;
    }
    double sum_sq = 0.0;
    for (size_t i = 0; i < count; ++i) {
        const double s = static_cast<double>(samples[i]);
        sum_sq += s * s;
    }
    const float instant = static_cast<float>(
        std::sqrt(sum_sq / static_cast<double>(count)) / 32768.0);
    const float prev = level.load(std::memory_order_relaxed);
    // Fast attack (jump straight up), slow release (~0.9 decay per 10 ms block ≈ 150 ms fade).
    const float next = instant > prev ? instant : prev * 0.9f;
    level.store(next, std::memory_order_relaxed);
    // Stamp the block time so a reader can keep fading when blocks STOP arriving.
    updated_ms.store(MonotonicNowMs(), std::memory_order_relaxed);
}

// Read the smoothed level, continuing the ~0.9-per-10 ms release for wall-clock time elapsed
// since the last block. Without this a sender that STOPS sending audio (natural pause, Opus
// DTX, mute, or leaving) freezes the last level on the atomic and the remote "speaking" ring
// never clears. One block of grace absorbs the normal inter-block gap so active speech is not
// attenuated. Pure (non-mutating) — safe to call from the UI poll thread.
static inline float OmniReadSpeakingLevel(
    const std::atomic<float>& level, const std::atomic<int64_t>& updated_ms)
{
    const float stored = level.load(std::memory_order_relaxed);
    if (stored <= 0.0f) {
        return 0.0f;
    }
    const int64_t last = updated_ms.load(std::memory_order_relaxed);
    if (last <= 0) {
        return stored;
    }
    const int64_t now = MonotonicNowMs();
    const int64_t elapsed = now > last ? now - last : 0;
    // Grace covers the normal inter-block gap (10 ms APM / up to ~20 ms Opus frame + jitter) so a
    // still-flowing stream isn't attenuated; only a genuinely stalled one ages past it.
    constexpr int64_t kGraceMs = 30;
    if (elapsed <= kGraceMs) {
        return stored;
    }
    const float aged_blocks = static_cast<float>(elapsed - kGraceMs) / 10.0f;
    return stored * std::pow(0.9f, aged_blocks);
}

class OmniAudioCaptureImpl {
public:
    static std::unique_ptr<OmniAudioCaptureImpl> Create(
        int target_bitrate_bps,
        const std::wstring& device_id,
        std::string& error)
    {
        auto sender = OmniAudioSender::Create(target_bitrate_bps, &error);
        if (!sender) {
            return nullptr;
        }
        auto impl = std::unique_ptr<OmniAudioCaptureImpl>(new OmniAudioCaptureImpl());
        impl->sender_ = std::move(sender);
        {
            // Выключенный микрофон — здесь, а не в оболочке: кодировщик и уровень речи работают
            // как работали, в сеть не уходит ничего.
            OmniAudioCaptureImpl* owner = impl.get();
            impl->sender_->SetPacketOut([owner](const uint8_t* data, size_t size) {
                if (!owner->muted_.load(std::memory_order_relaxed)) {
                    owner->packet_sink_.Send(data, size);
                }
            });
        }
        impl->capture_ = std::make_unique<OmniWasapiCapture>();

        // Audio quality (2026-06-05): run the captured mic through the libwebrtc audio
        // processing module (APM) before encoding — high-pass filter + noise suppression
        // (high) + AGC2 adaptive digital gain + acoustic echo cancellation (AEC3). The AEC
        // reverse reference (far-end render audio) is fed by the playback session through the
        // shared g_echo_apm published below; AEC3 estimates the render→capture delay itself.
        // If the APM cannot be built we degrade gracefully to raw passthrough.
        webrtc::AudioProcessing::Config apm_config;
        apm_config.high_pass_filter.enabled = true;
        apm_config.noise_suppression.enabled = true;
        apm_config.noise_suppression.level =
            webrtc::AudioProcessing::Config::NoiseSuppression::kHigh;
        apm_config.gain_controller2.enabled = true;
        apm_config.gain_controller2.adaptive_digital.enabled = true;
        apm_config.echo_canceller.enabled = true;
        impl->apm_env_.emplace(MakeOmniEnvironment());
        impl->apm_ = webrtc::BuiltinAudioProcessingBuilder(apm_config).Build(*impl->apm_env_);
        if (impl->apm_) {
            // Publish as the active call's AEC reference so the playback session can feed the
            // reverse stream. Cleared in the destructor.
            PublishEchoApm(impl->apm_);
        }

        OmniAudioCaptureImpl* self = impl.get();
        const HRESULT hr = impl->capture_->Start(
            [self](const int16_t* samples, size_t sample_count) {
                self->ProcessAndForward(samples, sample_count);
            },
            device_id);
        if (FAILED(hr)) {
            // Map HRESULT into diagnostic — the value is rarely
            // actionable upstream but it does distinguish "no
            // endpoint" (ERROR_NOT_FOUND) from "access denied".
            char hr_text[32];
            std::snprintf(hr_text, sizeof(hr_text), "0x%08lX", static_cast<unsigned long>(hr));
            error = std::string("OmniWasapiCapture::Start failed hr=") + hr_text;
            return nullptr;
        }
        return impl;
    }

    ~OmniAudioCaptureImpl()
    {
        // Order matters: stop the capture worker first so the
        // closure into sender_->PushPcm has no caller. Only then
        // is it safe to drop sender_. unique_ptr destruction order
        // alone is reverse-declaration which would be capture
        // before sender (we want exactly that here since capture
        // is declared after sender), but an explicit reset()
        // documents the contract for future maintainers.
        if (capture_) {
            capture_->Stop();
        }
        // Stop being the global AEC reference before the APM is torn down (a playback session
        // that already copied the ref keeps it alive until its in-flight reverse call returns).
        ClearEchoApm(apm_.get());
        capture_.reset();
        sender_.reset();
    }

    void SetPacketSink(OmniWebRtcMediaPacketSink sink, void* context)
    {
        packet_sink_.Set(sink, context);
    }

    void SetMuted(bool muted) { muted_.store(muted, std::memory_order_relaxed); }

    uint64_t PacketsSent() const { return packet_sink_.handed(); }

    /// How long the raw device input has been pure digital zero: since the last sample that was
    /// not, or since capture started. Measured before noise suppression — after it, a quiet room
    /// and a dead device look the same.
    int64_t DigitalSilenceMs() const
    {
        const int64_t heard = raw_sound_ms_.load(std::memory_order_relaxed);
        const int64_t since = heard > 0 ? heard : started_ms_;
        return std::max<int64_t>(0, MonotonicNowMs() - since);
    }

    // Smoothed RMS of the mic audio being sent (0..1). The self tile's speaking ring polls this.
    float SpeakingLevel() const { return OmniReadSpeakingLevel(speaking_level_, speaking_level_updated_ms_); }

    /// One line describing what the echo canceller is actually doing, for the call
    /// diagnostics. Reports, in order of what you need to know when someone says "it howls":
    ///   apm=off              — the APM could not be built; capture is raw passthrough and
    ///                          there is NO echo cancellation at all
    ///   reverse_blocks=0     — nothing is feeding the far-end reference, so AEC3 has no idea
    ///                          what came out of the speaker and cannot cancel it
    ///   erle=<dB>            — how much echo it is actually removing (0 means: fed, but not
    ///                          converging — a delay or device-routing problem)
    ///   delay=<ms>           — the render→capture delay AEC3 settled on
    std::string EchoDiagnostics() const
    {
        if (apm_ == nullptr) {
            return "aec=off(apm_unavailable)";
        }
        const uint64_t reverse_blocks = g_echo_reverse_blocks.load(std::memory_order_relaxed);
        const webrtc::AudioProcessingStats stats = apm_->GetStatistics();
        std::ostringstream out;
        out << "aec=on reverse_blocks=" << reverse_blocks;
        // КАКИЕ УСТРОЙСТВА ОТКРЫТЫ — БЕЗ ЭТОГО «МИКРОФОН НЕ СЛЫШИТ» НЕ РАЗБИРАЕТСЯ.
        //
        // Windows держит два устройства по умолчанию; звонок открывал УСТРОЙСТВО СВЯЗИ
        // (eCommunications), а человек переключает обычное (с 2026-10-11 — обычное). Замер 2026-08-28 встал ровно на этом:
        // динамики переключены, будильник в комнате слышно, а звонок сидел на гарнитуре —
        // и увидеть это было нечем.
        if (capture_ != nullptr && !capture_->DeviceName().empty()) {
            out << " mic=\"" << JsonEscape(capture_->DeviceName()) << '"';
        }
        if (const std::string render_device = OmniRenderDeviceName(); !render_device.empty()) {
            out << " spkdev=\"" << JsonEscape(render_device) << '"';
        }
        if (stats.echo_return_loss.has_value()) {
            out << " erl=" << std::fixed << std::setprecision(1) << *stats.echo_return_loss;
        }
        if (stats.echo_return_loss_enhancement.has_value()) {
            out << " erle=" << std::fixed << std::setprecision(1)
                << *stats.echo_return_loss_enhancement;
        }
        if (stats.delay_ms.has_value()) {
            out << " delay=" << *stats.delay_ms << "ms";
        }
        if (stats.residual_echo_likelihood.has_value()) {
            out << " residual=" << std::fixed << std::setprecision(2)
                << *stats.residual_echo_likelihood;
        }
        return out.str();
    }

private:
    OmniAudioCaptureImpl() = default;

    static constexpr int kApmSampleRate = 48000;
    static constexpr size_t kApmChannels = 1;
    static constexpr size_t kApmBlockSamples = 480;  // APM block = 10 ms @ 48 kHz mono

    // Mic capture -> APM (HPF + NS + AGC2) -> opus encoder. The device delivers
    // variable-size chunks; the APM requires whole 10 ms blocks, so accumulate and process
    // block by block (the encoder re-accumulates into its own 20 ms opus frames). Touched
    // only from the capture worker thread, which is stopped before destruction.
    void ProcessAndForward(const int16_t* samples, size_t sample_count)
    {
        // Any sample off digital zero is a microphone hearing something: a real one always has a
        // noise floor. A device that only ever delivers zeros (a virtual mic with no source, a
        // hardware mute switch) is what the call screen warns about.
        for (size_t i = 0; i < sample_count; ++i) {
            if (samples[i] > 1 || samples[i] < -1) {
                raw_sound_ms_.store(MonotonicNowMs(), std::memory_order_relaxed);
                break;
            }
        }
        if (apm_ == nullptr) {
            OmniUpdateSpeakingLevel(speaking_level_, speaking_level_updated_ms_, samples, sample_count);
            sender_->PushPcm(samples, sample_count);
            return;
        }
        apm_accumulator_.insert(apm_accumulator_.end(), samples, samples + sample_count);
        const webrtc::StreamConfig stream_config(kApmSampleRate, kApmChannels);
        std::array<int16_t, kApmBlockSamples> processed{};
        while (apm_accumulator_.size() >= kApmBlockSamples) {
            const int err = apm_->ProcessStream(
                apm_accumulator_.data(), stream_config, stream_config, processed.data());
            const int16_t* out = (err == webrtc::AudioProcessing::kNoError)
                ? processed.data()
                : apm_accumulator_.data();
            // Level from the post-NS/AGC block we actually transmit — noise is suppressed, so
            // the speaking ring tracks voice rather than ambient hum.
            OmniUpdateSpeakingLevel(speaking_level_, speaking_level_updated_ms_, out, kApmBlockSamples);
            sender_->PushPcm(out, kApmBlockSamples);
            apm_accumulator_.erase(
                apm_accumulator_.begin(),
                apm_accumulator_.begin() + static_cast<std::ptrdiff_t>(kApmBlockSamples));
        }
    }

    // Declaration order: sender first (destroyed last), capture
    // second (destroyed first when ~OmniAudioCaptureImpl reaches
    // the implicit member dtors after the explicit resets above).
    // The APM env is declared before the APM so the APM (which references the env) is
    // destroyed first (members destruct in reverse declaration order).
    // The packet sink and the mute flag come before the sender: the sender's packet output
    // refers to both, so they must outlive it.
    OmniPacketSink packet_sink_;
    std::atomic<bool> muted_{false};
    std::unique_ptr<OmniAudioSender> sender_;
    std::unique_ptr<OmniWasapiCapture> capture_;
    std::optional<webrtc::Environment> apm_env_;
    webrtc::scoped_refptr<webrtc::AudioProcessing> apm_;
    std::vector<int16_t> apm_accumulator_;
    std::atomic<float> speaking_level_{0.0f};
    std::atomic<int64_t> speaking_level_updated_ms_{0};
    const int64_t started_ms_ = MonotonicNowMs();
    std::atomic<int64_t> raw_sound_ms_{0};
};
} // namespace
#endif

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_audio_capture_create(
    const char* config_json,
    OmniWebRtcMediaAudioCapture** out_handle,
    char* error_buffer,
    size_t error_buffer_len,
    size_t* required_error_len)
{
    if (out_handle == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }

    *out_handle = nullptr;

#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    const std::string_view json = config_json == nullptr
        ? std::string_view("{}")
        : std::string_view(config_json);
    const int requested_bitrate = FindJsonInt(json, "target_bitrate_bps").value_or(32000);
    const int clamped_bitrate = ClampInt(requested_bitrate, 6000, 510000);
    const std::wstring device_id = Utf8ToWide(FindJsonString(json, "device_id").value_or(""));

    std::string error;
    std::unique_ptr<OmniAudioCaptureImpl> impl =
        OmniAudioCaptureImpl::Create(clamped_bitrate, device_id, error);
    if (!impl) {
        (void)CopyString(error, error_buffer, error_buffer_len, required_error_len);
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }

    auto handle = new OmniWebRtcMediaAudioCapture();
    handle->impl = impl.release();
    *out_handle = handle;
    if (required_error_len != nullptr) { *required_error_len = 0; }
    if (error_buffer != nullptr && error_buffer_len > 0) { error_buffer[0] = '\0'; }
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    (void)config_json;
    const OmniWebRtcMediaStatus copy_status = CopyString(
        kNoBackendSessionNotImplemented,
        error_buffer,
        error_buffer_len,
        required_error_len);
    return copy_status == OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR
        ? copy_status
        : OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" void omni_webrtc_media_audio_capture_destroy(
    OmniWebRtcMediaAudioCapture* handle)
{
    if (handle == nullptr) { return; }
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (handle->impl != nullptr) {
        delete static_cast<OmniAudioCaptureImpl*>(handle->impl);
        handle->impl = nullptr;
    }
#endif
    delete handle;
}


extern "C" OmniWebRtcMediaStatus omni_webrtc_media_audio_capture_set_packet_sink(
    OmniWebRtcMediaAudioCapture* handle,
    OmniWebRtcMediaPacketSink sink,
    void* context)
{
    if (handle == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (handle->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }
    static_cast<OmniAudioCaptureImpl*>(handle->impl)->SetPacketSink(sink, context);
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    (void)sink;
    (void)context;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_audio_capture_set_muted(
    OmniWebRtcMediaAudioCapture* handle,
    int32_t muted)
{
    if (handle == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (handle->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }
    static_cast<OmniAudioCaptureImpl*>(handle->impl)->SetMuted(muted != 0);
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    (void)muted;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_audio_capture_packets_sent(
    OmniWebRtcMediaAudioCapture* handle,
    uint64_t* out_sent)
{
    if (handle == nullptr || out_sent == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
    *out_sent = 0;
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (handle->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }
    *out_sent = static_cast<OmniAudioCaptureImpl*>(handle->impl)->PacketsSent();
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

// How long the microphone's raw input has been pure digital zero (ABI 21): the call screen says
// "the microphone hears nothing" when it stays so while the mic is on.
extern "C" OmniWebRtcMediaStatus omni_webrtc_media_audio_capture_digital_silence_ms(
    OmniWebRtcMediaAudioCapture* handle,
    int64_t* out_ms)
{
    if (handle == nullptr || out_ms == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (handle->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }
    *out_ms = static_cast<OmniAudioCaptureImpl*>(handle->impl)->DigitalSilenceMs();
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    *out_ms = 0;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

// Group-call speaking indicator (2026-07-11): smoothed RMS (0..1) of the mic audio currently
// being sent. The self tile polls this to light its "speaking" ring; 0 when muted/no-signal.
extern "C" OmniWebRtcMediaStatus omni_webrtc_media_audio_capture_level(
    OmniWebRtcMediaAudioCapture* handle,
    float* out_level)
{
    if (handle == nullptr || out_level == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (handle->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }
    *out_level = static_cast<OmniAudioCaptureImpl*>(handle->impl)->SpeakingLevel();
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    *out_level = 0.0f;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_audio_capture_echo_diagnostics(
    OmniWebRtcMediaAudioCapture* handle,
    char* buffer,
    size_t buffer_len,
    size_t* out_written)
{
    if (handle == nullptr || buffer == nullptr || buffer_len == 0 || out_written == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (handle->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }
    const std::string text =
        static_cast<OmniAudioCaptureImpl*>(handle->impl)->EchoDiagnostics();
    if (text.size() + 1 > buffer_len) {
        *out_written = text.size() + 1;
        return OMNI_WEBRTC_MEDIA_STATUS_BUFFER_TOO_SMALL;
    }
    std::memcpy(buffer, text.c_str(), text.size() + 1);
    *out_written = text.size() + 1;
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    const char* text = "aec=unavailable(no_libwebrtc)";
    const size_t needed = std::strlen(text) + 1;
    if (needed > buffer_len) {
        *out_written = needed;
        return OMNI_WEBRTC_MEDIA_STATUS_BUFFER_TOO_SMALL;
    }
    std::memcpy(buffer, text, needed);
    *out_written = needed;
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#endif
}

#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
namespace {
// Активные устройства одного направления в JSON-массив. Пустой массив, если перечислить нельзя:
// выбор устройства — удобство, его отсутствие не должно ломать настройки.
void AppendAudioEndpointsJson(IMMDeviceEnumerator* enumerator, EDataFlow flow, std::ostringstream& out)
{
    out << '[';
    std::wstring default_id;
    {
        Microsoft::WRL::ComPtr<IMMDevice> fallback;
        LPWSTR id = nullptr;
        if (SUCCEEDED(OpenAudioEndpoint(enumerator, flow, {}, fallback.GetAddressOf())) &&
            SUCCEEDED(fallback->GetId(&id)) && id != nullptr) {
            default_id = id;
            ::CoTaskMemFree(id);
        }
    }
    Microsoft::WRL::ComPtr<IMMDeviceCollection> devices;
    UINT count = 0;
    if (SUCCEEDED(enumerator->EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE, devices.GetAddressOf())) &&
        SUCCEEDED(devices->GetCount(&count))) {
        bool first = true;
        for (UINT index = 0; index < count; ++index) {
            Microsoft::WRL::ComPtr<IMMDevice> device;
            LPWSTR id = nullptr;
            if (FAILED(devices->Item(index, device.GetAddressOf())) ||
                FAILED(device->GetId(&id)) || id == nullptr) {
                continue;
            }
            const bool is_default = default_id == id;
            const std::string id_utf8 = WideToUtf8(id);
            ::CoTaskMemFree(id);
            if (!first) {
                out << ',';
            }
            first = false;
            out << R"({"id":")" << JsonEscape(id_utf8)
                << R"(","name":")" << JsonEscape(ReadAudioEndpointName(device.Get()))
                << R"(","default":)" << (is_default ? "true" : "false") << '}';
        }
    }
    out << ']';
}
} // namespace
#endif

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_audio_list_devices(
    char* buffer,
    size_t buffer_len,
    size_t* out_written)
{
    if (buffer == nullptr || buffer_len == 0 || out_written == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    // Зовут и с UI-потока (STA): тогда CoInitializeEx отвечает RPC_E_CHANGED_MODE, COM уже есть.
    const HRESULT init = ::CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    std::ostringstream json;
    {
        Microsoft::WRL::ComPtr<IMMDeviceEnumerator> enumerator;
        const HRESULT hr = ::CoCreateInstance(
            __uuidof(MMDeviceEnumerator),
            nullptr,
            CLSCTX_ALL,
            __uuidof(IMMDeviceEnumerator),
            reinterpret_cast<void**>(enumerator.GetAddressOf()));
        if (FAILED(hr)) {
            json << R"({"capture":[],"render":[]})";
        } else {
            json << R"({"capture":)";
            AppendAudioEndpointsJson(enumerator.Get(), eCapture, json);
            json << R"(,"render":)";
            AppendAudioEndpointsJson(enumerator.Get(), eRender, json);
            json << '}';
        }
    }
    if (SUCCEEDED(init)) {
        ::CoUninitialize();
    }
    const std::string text = json.str();
#else
    const std::string text = R"({"capture":[],"render":[]})";
#endif
    *out_written = text.size() + 1;
    if (text.size() + 1 > buffer_len) {
        return OMNI_WEBRTC_MEDIA_STATUS_BUFFER_TOO_SMALL;
    }
    std::memcpy(buffer, text.c_str(), text.size() + 1);
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
}

// Audio Phase 2b-step3 (2026-05-23): production FFI surface for the
// audio receive+playback pipeline. Mirrors the audio capture exports
// (create/destroy/push) but inverts the direction — caller PushRtpPacket
// with packets received from the wire, and the playback owns a WASAPI
// render endpoint that emits the decoded PCM. Independent from
// OmniWebRtcMediaAudioCapture: full duplex (Phase 2c) instantiates one
// of each.
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
namespace {
class OmniAudioPlaybackImpl {
public:
    // render_enabled=false makes this a decode-only level probe: it decodes the sender's Opus to
    // PCM and tracks the speaking level, but opens no WASAPI render endpoint and feeds no AEC
    // reverse reference. Used for per-sender group-call active-speaker levels alongside the single
    // shared render playback, so measuring N senders' levels never doubles audio or pollutes AEC.
    static std::unique_ptr<OmniAudioPlaybackImpl> Create(
        bool render_enabled,
        const std::wstring& device_id,
        std::string& error)
    {
        auto impl = std::unique_ptr<OmniAudioPlaybackImpl>(new OmniAudioPlaybackImpl());
        impl->receiver_ = OmniNetEqAudioReceiver::Create(&error);
        if (!impl->receiver_) {
            return nullptr;
        }
        OmniAudioPlaybackImpl* self = impl.get();
        if (render_enabled) {
            impl->render_ = std::make_unique<OmniWasapiRender>();
            // Устройство само тянет звук из NetEq. Опорный сигнал эхоподавителя снимается в момент
            // отдачи сэмплов устройству. `this` пережить воркер не может: деструктор первым делом
            // зовёт render_->Stop(), а тот join-ит воркер.
            impl->render_->SetPcmSource([self](int16_t* dst, size_t sample_count) {
                self->FillForDevice(dst, sample_count);
            });
            impl->render_->SetRenderedSamplesObserver(
                [self](const int16_t* samples, size_t sample_count) {
                    self->FeedReverseFromDevice(samples, sample_count);
                });
            const HRESULT hr = impl->render_->Start(device_id);
            if (FAILED(hr)) {
                char hr_text[32];
                std::snprintf(hr_text, sizeof(hr_text), "0x%08lX", static_cast<unsigned long>(hr));
                error = std::string("OmniWasapiRender::Start failed hr=") + hr_text;
                return nullptr;
            }
        }
        return impl;
    }

    ~OmniAudioPlaybackImpl()
    {
        // Stop the render worker first: it pulls from receiver_.
        if (render_) {
            render_->Stop();
        }
        render_.reset();
        receiver_.reset();
    }

    void PushRtpPacket(const uint8_t* packet, size_t packet_len)
    {
        const auto result = receiver_->InsertRtpPacket(packet, packet_len);
        if (result == OmniNetEqAudioReceiver::InsertResult::kAccepted) {
            packets_accepted_.fetch_add(1, std::memory_order_relaxed);
        } else {
            packets_dropped_.fetch_add(1, std::memory_order_relaxed);
        }
    }

    OmniWebRtcMediaAudioJitterStats JitterStats() const
    {
        const webrtc::NetEqNetworkStatistics now = receiver_->CurrentStatistics();
        const webrtc::NetEqLifetimeStatistics lifetime = receiver_->LifetimeStatistics();
        OmniWebRtcMediaAudioJitterStats stats{};
        stats.buffer_ms = now.current_buffer_size_ms;
        stats.target_ms = now.preferred_buffer_size_ms;
        stats.concealed_ms = lifetime.concealed_samples / 48;
        stats.accelerated_ms = lifetime.removed_samples_for_acceleration / 48;
        stats.decelerated_ms = lifetime.inserted_samples_for_deceleration / 48;
        return stats;
    }

    uint64_t PacketsAccepted() const
    {
        return packets_accepted_.load(std::memory_order_relaxed);
    }

    uint64_t PacketsDropped() const
    {
        return packets_dropped_.load(std::memory_order_relaxed);
    }

    // Lipsync: the video session's playout target becomes NetEq's minimum delay, so audio does not
    // run ahead of video; NetEq still adds jitter headroom on top when the path needs it. Clamped
    // at 500 ms (past that it is jitter buffering, not lipsync). Returns the value applied.
    uint32_t SetTargetPlayoutDelayMs(uint32_t delay_ms)
    {
        const uint32_t clamped = std::min<uint32_t>(delay_ms, 500u);
        receiver_->SetMinimumDelayMs(static_cast<int>(clamped));
        return clamped;
    }

private:
    OmniAudioPlaybackImpl() = default;

    static constexpr int kReverseSampleRate = 48000;
    static constexpr size_t kReverseChannels = 1;
    static constexpr size_t kReverseBlockSamples = 480;  // 10 ms @ 48 kHz mono

    // Отдать устройству ровно `sample_count` сэмплов: NetEq отдаёт по 10 мс, устройство просит
    // сколько у него свободно, остаток ждёт следующего запроса. Зовётся только воркером устройства.
    void FillForDevice(int16_t* dst, size_t sample_count)
    {
        std::array<int16_t, OmniNetEqAudioReceiver::kPullSamples> block{};
        while (pending_.size() < sample_count) {
            PullBlock(block.data());
            pending_.insert(pending_.end(), block.begin(), block.end());
        }
        std::copy(pending_.begin(), pending_.begin() + static_cast<std::ptrdiff_t>(sample_count), dst);
        pending_.erase(pending_.begin(), pending_.begin() + static_cast<std::ptrdiff_t>(sample_count));
    }

    // 10 мс из NetEq, с уровнем речи по тому, что реально звучит.
    void PullBlock(int16_t* block)
    {
        receiver_->Pull10Ms(block);
        OmniUpdateSpeakingLevel(speaking_level_, speaking_level_updated_ms_, block,
                                OmniNetEqAudioReceiver::kPullSamples);
    }

    // Зонд уровня (без устройства) тянуть некому — тянет сам, при чтении уровня: столько блоков,
    // сколько прошло 10-миллисекундных тактов с прошлого раза (не больше секунды за раз).
    void CatchUpProbe()
    {
        const int64_t now_ms = std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now().time_since_epoch()).count();
        std::lock_guard<std::mutex> lock(probe_mutex_);
        if (probe_pulled_until_ms_ == 0) {
            probe_pulled_until_ms_ = now_ms;
            return;
        }
        std::array<int16_t, OmniNetEqAudioReceiver::kPullSamples> block{};
        int steps = 0;
        while (probe_pulled_until_ms_ + 10 <= now_ms && steps < 100) {
            PullBlock(block.data());
            probe_pulled_until_ms_ += 10;
            ++steps;
        }
        if (probe_pulled_until_ms_ + 10 <= now_ms) {
            probe_pulled_until_ms_ = now_ms;  // долго не спрашивали — прошлое не догоняем
        }
    }

    // ОПОРНЫЙ СИГНАЛ ЭХОПОДАВИТЕЛЯ СНИМАЕТСЯ ТАМ, ГДЕ ЗВУК УХОДИТ В УСТРОЙСТВО (2026-08-27).
    //
    // Раньше `ProcessReverseStream` звался прямо тут — то есть в момент ДЕКОДИРОВАНИЯ. Между
    // этим моментом и динамиком стоят две задержки: липсинк (`SetTargetPlayoutDelayMs`, до
    // 500 мс) и буфер WASAPI (~60 мс). AEC3 ищет выравнивание сам, но окно поиска конечно, и при
    // липсинке в сотни миллисекунд опорный сигнал уходит за его край. Живой замер это и
    // показывал: `erl=-30.0 delay=0ms` читается как «выравнивание не найдено».
    //
    // Взаимодействие двух фич: липсинк НАМЕРЕННО задерживает воспроизведение, и ровно на эту
    // величину разъезжался опорный сигнал. Теперь он снимается там, где сэмплы реально уходят в
    // устройство — через наблюдателя `SetRenderedSamplesObserver`.
    // См. docs/bugs/echo-was-invisible.md.
    /// Опорный сигнал: то, что ПРЯМО СЕЙЧАС ушло в динамик.
    ///
    /// Зовётся из воркера воспроизведения (один поток), поэтому накопитель без замка. Блоки —
    /// ровно по 10 мс, как требует APM; остаток ждёт следующего вызова, и тишина добивки идёт
    /// наравне со звуком: разрыв в опорном сигнале эхоподавитель прочтёт как рассинхрон.
    void FeedReverseFromDevice(const int16_t* samples, size_t sample_count)
    {
        webrtc::scoped_refptr<webrtc::AudioProcessing> apm = AcquireEchoApm();
        if (!apm || samples == nullptr || sample_count == 0) {
            return;
        }
        reverse_accumulator_.insert(reverse_accumulator_.end(), samples, samples + sample_count);
        const webrtc::StreamConfig stream_config(kReverseSampleRate, kReverseChannels);
        std::array<int16_t, kReverseBlockSamples> processed{};
        while (reverse_accumulator_.size() >= kReverseBlockSamples) {
            apm->ProcessReverseStream(
                reverse_accumulator_.data(), stream_config, stream_config, processed.data());
            g_echo_reverse_blocks.fetch_add(1, std::memory_order_relaxed);
            reverse_accumulator_.erase(
                reverse_accumulator_.begin(),
                reverse_accumulator_.begin() + static_cast<std::ptrdiff_t>(kReverseBlockSamples));
        }
    }

    // Declaration order: receiver before render, so render (which pulls from it) is destroyed
    // first; the explicit Stop in the dtor already joins the worker before that.
    std::unique_ptr<OmniNetEqAudioReceiver> receiver_;
    std::unique_ptr<OmniWasapiRender> render_;
    std::vector<int16_t> pending_;  // pulled but not yet taken by the device
    std::mutex probe_mutex_;
    int64_t probe_pulled_until_ms_ = 0;
    std::vector<int16_t> reverse_accumulator_;
    std::atomic<uint64_t> packets_accepted_{0};
    std::atomic<uint64_t> packets_dropped_{0};
    std::atomic<float> speaking_level_{0.0f};
    std::atomic<int64_t> speaking_level_updated_ms_{0};

public:
    // Smoothed RMS (0..1) of this sender's decoded audio — the remote tile's speaking ring.
    // Read via the aging helper so the ring fades when this sender STOPS sending (pause / DTX /
    // mute / left) instead of freezing on the last decoded block.
    float SpeakingLevel()
    {
        if (!render_) {
            CatchUpProbe();
        }
        return OmniReadSpeakingLevel(speaking_level_, speaking_level_updated_ms_);
    }

private:
};
} // namespace
#endif

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_audio_playback_create(
    const char* config_json,
    OmniWebRtcMediaAudioPlayback** out_handle,
    char* error_buffer,
    size_t error_buffer_len,
    size_t* required_error_len)
{
    if (out_handle == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
    *out_handle = nullptr;

#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    // "render": 1 (default) = full render+AEC playback; 0 = decode-only level probe (per-sender
    // group active-speaker level, no WASAPI endpoint). "device_id": the chosen speakers.
    const std::string_view json = config_json == nullptr
        ? std::string_view("{}")
        : std::string_view(config_json);
    const bool render_enabled = FindJsonInt(json, "render").value_or(1) != 0;
    const std::wstring device_id = Utf8ToWide(FindJsonString(json, "device_id").value_or(""));
    std::string error;
    std::unique_ptr<OmniAudioPlaybackImpl> impl =
        OmniAudioPlaybackImpl::Create(render_enabled, device_id, error);
    if (!impl) {
        (void)CopyString(error, error_buffer, error_buffer_len, required_error_len);
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }

    auto handle = new OmniWebRtcMediaAudioPlayback();
    handle->impl = impl.release();
    *out_handle = handle;
    if (required_error_len != nullptr) { *required_error_len = 0; }
    if (error_buffer != nullptr && error_buffer_len > 0) { error_buffer[0] = '\0'; }
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    (void)config_json;
    const OmniWebRtcMediaStatus copy_status = CopyString(
        kNoBackendSessionNotImplemented,
        error_buffer,
        error_buffer_len,
        required_error_len);
    return copy_status == OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR
        ? copy_status
        : OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" void omni_webrtc_media_audio_playback_destroy(
    OmniWebRtcMediaAudioPlayback* handle)
{
    if (handle == nullptr) { return; }
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (handle->impl != nullptr) {
        delete static_cast<OmniAudioPlaybackImpl*>(handle->impl);
        handle->impl = nullptr;
    }
#endif
    delete handle;
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_audio_playback_push_rtp_packet(
    OmniWebRtcMediaAudioPlayback* handle,
    const uint8_t* packet,
    size_t packet_len)
{
    if (handle == nullptr || packet == nullptr || packet_len == 0) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (handle->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }
    auto* impl = static_cast<OmniAudioPlaybackImpl*>(handle->impl);
    // PushResult kAccepted is the only OK case; everything else is a
    // silent drop on the C++ side. The C ABI flattens all drop reasons
    // into OK — the caller doesn't need that granularity, and an error
    // status would make the demux loop in C# look like it broke when
    // it just routed a stray packet. PT/SSRC mismatch is normal on
    // the shared video+audio datagram channel (video packets arrive
    // here, get dropped, video receiver picks them up separately).
    (void)impl->PushRtpPacket(packet, packet_len);
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    (void)packet; (void)packet_len;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_audio_playback_read_counters(
    OmniWebRtcMediaAudioPlayback* handle,
    uint64_t* packets_accepted,
    uint64_t* packets_dropped)
{
    if (handle == nullptr || packets_accepted == nullptr || packets_dropped == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (handle->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }
    auto* impl = static_cast<OmniAudioPlaybackImpl*>(handle->impl);
    *packets_accepted = impl->PacketsAccepted();
    *packets_dropped = impl->PacketsDropped();
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    *packets_accepted = 0;
    *packets_dropped = 0;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

// Group-call active-speaker (2026-07-11): smoothed RMS (0..1) of this sender's decoded audio.
// The per-sender remote tile polls this to light its speaking ring / auto-focus the loudest.
extern "C" OmniWebRtcMediaStatus omni_webrtc_media_audio_playback_level(
    OmniWebRtcMediaAudioPlayback* handle,
    float* out_level)
{
    if (handle == nullptr || out_level == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (handle->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }
    *out_level = static_cast<OmniAudioPlaybackImpl*>(handle->impl)->SpeakingLevel();
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    *out_level = 0.0f;
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_audio_playback_jitter_stats(
    OmniWebRtcMediaAudioPlayback* handle,
    OmniWebRtcMediaAudioJitterStats* out_stats)
{
    if (handle == nullptr || out_stats == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
    *out_stats = OmniWebRtcMediaAudioJitterStats{};
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (handle->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }
    *out_stats = static_cast<OmniAudioPlaybackImpl*>(handle->impl)->JitterStats();
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_audio_playback_set_target_playout_delay_ms(
    OmniWebRtcMediaAudioPlayback* handle,
    uint32_t delay_ms,
    uint32_t* applied_delay_ms)
{
    if (handle == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
#if defined(_WIN32) && defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (handle->impl == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
    }
    auto* impl = static_cast<OmniAudioPlaybackImpl*>(handle->impl);
    const uint32_t applied = impl->SetTargetPlayoutDelayMs(delay_ms);
    if (applied_delay_ms != nullptr) {
        *applied_delay_ms = applied;
    }
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
#else
    (void)delay_ms;
    if (applied_delay_ms != nullptr) { *applied_delay_ms = 0; }
    return OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED;
#endif
}

// РАЗБОРЩИК ВХОДЯЩИХ МЕДИА (ABI 14, 2026-09-27).
//
// Ядро отдаёт сюда каждую входящую датаграмму звонка сразу, на своём потоке (задача mesh, читающая
// соединение): omni_realtime_media_set_inbound_sink(…, omni_webrtc_media_router_deliver, router).
// Раньше датаграммы копились в очереди ядра, оболочка выгребала их таймером (~15.6 мс на Windows) и
// раскладывала сама — время прихода для регулятора было тактом выборки, а не приходом. См.
// docs/bugs/media-rode-the-shells-timer.md.
//
// Раскладка:
//   1:1   — отчёт о доставке (RTCP 205/11) отправляющей сессии; прочий RTCP (NACK, PLI, RR, SR)
//           ОБЕИМ сессиям видео, как Call::DeliverRtcpPacket раздаёт его всем потокам (каждый
//           модуль берёт своё по SSRC); RTP приёмной сессии видео и воспроизведению звука —
//           каждая сама отбрасывает не своё (SSRC / тип нагрузки);
//   group — первые 8 байт метка отправителя: звук в общее воспроизведение, видео и уровень речи
//           в сессии этого отправителя. Метка из одних 0xFF — не отправитель, а ОТЧЁТ участника о
//           НАШЕМ потоке (ядро, GROUP_FEEDBACK_SHELL_TAG): он отправляющей сессии, как в 1:1.
// Сессии строит оболочка (там согласованный формат); здесь только запоминается, что приёмник
// нужен (take_requests), — пакеты до его появления теряются, как и раньше.
//
// Всё под одним замком: set_* возвращается, когда ни одной раздачи в полёте нет, и после этого
// отключённую сессию можно разрушать.
struct OmniWebRtcMediaRouter {
    static constexpr size_t kGroupSenderTagBytes = 8;
    static constexpr size_t kMaxPacketBytes = 1500;
    using GroupTag = std::array<uint8_t, kGroupSenderTagBytes>;
    struct GroupSender {
        OmniWebRtcMediaSession* receiver = nullptr;
        OmniWebRtcMediaAudioPlayback* level_probe = nullptr;
        // The video payload type the receiver was built for (the sender's latest when it was wired;
        // H.264's when no video had come yet), and the one already re-requested — once per change.
        uint8_t wired_pt = 0;
        uint8_t requested_pt = 0;
    };

    std::mutex mutex;
    OmniWebRtcMediaSession* sender = nullptr;
    OmniWebRtcMediaSession* receiver = nullptr;
    OmniWebRtcMediaAudioPlayback* playback = nullptr;
    bool group_mode = false;
    std::map<GroupTag, GroupSender> group_senders;
    bool video_receiver_wanted = false;
    std::vector<GroupTag> group_senders_wanted;
    // Each group sender's latest video payload type: a sender picks its own codec for the group
    // (per-pair-video-codec-selection.md §9, phase 4a), and its receiver is built for it.
    std::map<GroupTag, uint8_t> group_sender_video_pt;
    uint64_t delivered = 0;
    uint64_t feedback = 0;
    uint64_t dropped = 0;

    // Сессия видео принимает пакеты пачкой с длиной впереди; здесь пачка из одного.
    static void PushVideo(OmniWebRtcMediaSession* session, const uint8_t* packet, size_t size)
    {
        if (size > kMaxPacketBytes) {
            return;
        }
        std::array<uint8_t, 2 + kMaxPacketBytes> batch{};
        batch[0] = static_cast<uint8_t>(size & 0xFFu);
        batch[1] = static_cast<uint8_t>((size >> 8) & 0xFFu);
        std::memcpy(batch.data() + 2, packet, size);
        (void)omni_webrtc_media_session_push_rtp_packets(session, batch.data(), size + 2, 1);
    }

    static bool IsOurFeedbackTag(const GroupTag& tag)
    {
        return std::all_of(tag.begin(), tag.end(), [](uint8_t b) { return b == 0xFFu; });
    }

    static bool IsCongestionControlFeedback(const uint8_t* packet, size_t size)
    {
        return size >= 2 && packet[1] == 205 && (packet[0] & 0x1Fu) == 11;
    }

    void DeliverUnsafe(const uint8_t* packet, size_t size)
    {
        ++delivered;
        if (group_mode) {
            if (size <= kGroupSenderTagBytes) {
                ++dropped;
                return;
            }
            GroupTag tag{};
            std::memcpy(tag.data(), packet, kGroupSenderTagBytes);
            const uint8_t* rtp = packet + kGroupSenderTagBytes;
            const size_t rtp_size = size - kGroupSenderTagBytes;
            if (IsOurFeedbackTag(tag)) {
                // Отчёт о нашем потоке от следующего узла (или просьба о повторе от любого):
                // отправляющей половине, и больше никому. Ядро уже отфильтровало, кто что шлёт.
                if (IsCongestionControlFeedback(rtp, rtp_size)) {
                    ++feedback;
                    if (sender != nullptr) {
                        (void)omni_webrtc_media_session_apply_transport_feedback(sender, rtp, rtp_size);
                    }
                } else if (sender != nullptr && LooksLikeRtcp(rtp, rtp_size)) {
                    PushVideo(sender, rtp, rtp_size);
                }
                return;
            }
            if (playback != nullptr) {
                (void)omni_webrtc_media_audio_playback_push_rtp_packet(playback, rtp, rtp_size);
            }
            // RTCP ОТПРАВИТЕЛЕЙ ГРУППЫ (SR и т.п.) ОТПРАВЛЯЮЩЕЙ СЕССИИ НЕ ОТДАЁТСЯ — НАМЕРЕННО. У всех
            // отправителей группы один SSRC видео, и NACK, адресованный одному, заставил бы повторять
            // всех. Отчёты о НАШЕМ потоке приходят отдельно, под меткой 0xFF… (выше): ядро шлёт
            // отчёт приёмника прямо узлу того отправителя, о ком он.
            const bool is_video = rtp_size >= 2 && IsVideoPayloadType(rtp[1] & 0x7Fu);
            if (is_video) {
                group_sender_video_pt[tag] = static_cast<uint8_t>(rtp[1] & 0x7Fu);
            }
            const auto found = group_senders.find(tag);
            if (found == group_senders.end()) {
                WantGroupSender(tag);
                return;
            }
            if (is_video) {
                const uint8_t pt = static_cast<uint8_t>(rtp[1] & 0x7Fu);
                const uint8_t wired = found->second.wired_pt != 0
                    ? found->second.wired_pt
                    : NativeVideoCodecPayloadType(NativeVideoCodec::H264);
                // The sender switched codec (or its first video is not H.264): ask for a receiver
                // built for it. Until then this one gets the packets and fails them — briefly.
                if (pt != wired && pt != found->second.requested_pt) {
                    found->second.requested_pt = pt;
                    WantGroupSender(tag);
                }
            }
            if (found->second.receiver != nullptr) {
                PushVideo(found->second.receiver, rtp, rtp_size);
            }
            if (found->second.level_probe != nullptr) {
                (void)omni_webrtc_media_audio_playback_push_rtp_packet(found->second.level_probe, rtp, rtp_size);
            }
            return;
        }

        if (IsCongestionControlFeedback(packet, size)) {
            // Отчёт рассказывает, что дошло из НАШИХ пакетов, — он отправляющей половине.
            ++feedback;
            if (sender != nullptr) {
                (void)omni_webrtc_media_session_apply_transport_feedback(sender, packet, size);
            }
            return;
        }
        // ПРОСЬБЫ И ОТЧЁТЫ СОБЕСЕДНИКА — ОБЕИМ ПОЛОВИНАМ. До ABI 16 весь прочий RTCP уходил только
        // приёмной сессии, а NACK и PLI адресованы отправляющей: они ехали структурными событиями
        // мимо этого пути и разницы не было. Теперь едут здесь.
        if (LooksLikeRtcp(packet, size)) {
            if (sender != nullptr) {
                PushVideo(sender, packet, size);
            }
            if (receiver != nullptr) {
                PushVideo(receiver, packet, size);
            }
            return;
        }
        if (receiver == nullptr && size >= 2 && IsVideoPayloadType(packet[1] & 0x7Fu)) {
            video_receiver_wanted = true;
        }
        if (receiver != nullptr) {
            PushVideo(receiver, packet, size);
        }
        if (playback != nullptr) {
            (void)omni_webrtc_media_audio_playback_push_rtp_packet(playback, packet, size);
        }
    }

    void WantGroupSender(const GroupTag& tag)
    {
        if (group_senders_wanted.size() < OMNI_WEBRTC_MEDIA_ROUTER_MAX_WANTED_GROUP_SENDERS &&
            std::find(group_senders_wanted.begin(), group_senders_wanted.end(), tag) == group_senders_wanted.end()) {
            group_senders_wanted.push_back(tag);
        }
    }

    // Any of the video codecs' payload types (the codec table above): an AV1 or HEVC stream must
    // ask for a decoder exactly like H.264 — with only 102 here the first AV1 call carried 230
    // packets into a receiver that was never built (live 2026-10-13).
    static bool IsVideoPayloadType(uint8_t payload_type)
    {
        for (NativeVideoCodec codec : {NativeVideoCodec::H264, NativeVideoCodec::H265, NativeVideoCodec::Av1}) {
            if (payload_type == NativeVideoCodecPayloadType(codec)) {
                return true;
            }
        }
        return false;
    }
};

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_router_create(OmniWebRtcMediaRouter** out_router)
{
    if (out_router == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
    *out_router = new (std::nothrow) OmniWebRtcMediaRouter();
    return *out_router != nullptr ? OMNI_WEBRTC_MEDIA_STATUS_OK : OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR;
}

extern "C" void omni_webrtc_media_router_destroy(OmniWebRtcMediaRouter* router)
{
    delete router;
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_router_set_sender(
    OmniWebRtcMediaRouter* router,
    OmniWebRtcMediaSession* sender)
{
    if (router == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
    std::lock_guard<std::mutex> lock(router->mutex);
    router->sender = sender;
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_router_set_receiver(
    OmniWebRtcMediaRouter* router,
    OmniWebRtcMediaSession* receiver)
{
    if (router == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
    std::lock_guard<std::mutex> lock(router->mutex);
    router->receiver = receiver;
    if (receiver != nullptr) {
        router->video_receiver_wanted = false;
    }
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_router_set_audio_playback(
    OmniWebRtcMediaRouter* router,
    OmniWebRtcMediaAudioPlayback* playback)
{
    if (router == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
    std::lock_guard<std::mutex> lock(router->mutex);
    router->playback = playback;
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_router_set_group_mode(
    OmniWebRtcMediaRouter* router,
    int32_t group_mode)
{
    if (router == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
    std::lock_guard<std::mutex> lock(router->mutex);
    router->group_mode = group_mode != 0;
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_router_set_group_sender(
    OmniWebRtcMediaRouter* router,
    const uint8_t* sender_tag,
    OmniWebRtcMediaSession* receiver,
    OmniWebRtcMediaAudioPlayback* level_probe)
{
    if (router == nullptr || sender_tag == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
    OmniWebRtcMediaRouter::GroupTag tag{};
    std::memcpy(tag.data(), sender_tag, tag.size());
    std::lock_guard<std::mutex> lock(router->mutex);
    if (receiver == nullptr && level_probe == nullptr) {
        router->group_senders.erase(tag);
        router->group_sender_video_pt.erase(tag);
    } else {
        const auto pt = router->group_sender_video_pt.find(tag);
        OmniWebRtcMediaRouter::GroupSender wired{receiver, level_probe};
        wired.wired_pt = pt != router->group_sender_video_pt.end() ? pt->second : 0;
        router->group_senders[tag] = wired;
        router->group_senders_wanted.erase(
            std::remove(router->group_senders_wanted.begin(), router->group_senders_wanted.end(), tag),
            router->group_senders_wanted.end());
    }
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_router_take_requests(
    OmniWebRtcMediaRouter* router,
    OmniWebRtcMediaRouterRequests* out_requests)
{
    if (router == nullptr || out_requests == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
    *out_requests = OmniWebRtcMediaRouterRequests{};
    std::lock_guard<std::mutex> lock(router->mutex);
    out_requests->video_receiver_wanted = router->video_receiver_wanted ? 1 : 0;
    router->video_receiver_wanted = false;
    out_requests->group_sender_count = static_cast<uint32_t>(router->group_senders_wanted.size());
    for (size_t i = 0; i < router->group_senders_wanted.size(); ++i) {
        std::memcpy(out_requests->group_sender_tags[i], router->group_senders_wanted[i].data(),
                    OmniWebRtcMediaRouter::kGroupSenderTagBytes);
        const auto pt = router->group_sender_video_pt.find(router->group_senders_wanted[i]);
        out_requests->group_sender_video_pt[i] = pt != router->group_sender_video_pt.end() ? pt->second : 0;
    }
    router->group_senders_wanted.clear();
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
}

extern "C" OmniWebRtcMediaStatus omni_webrtc_media_router_counters(
    OmniWebRtcMediaRouter* router,
    uint64_t* out_delivered,
    uint64_t* out_feedback,
    uint64_t* out_dropped)
{
    if (router == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
    std::lock_guard<std::mutex> lock(router->mutex);
    if (out_delivered != nullptr) { *out_delivered = router->delivered; }
    if (out_feedback != nullptr) { *out_feedback = router->feedback; }
    if (out_dropped != nullptr) { *out_dropped = router->dropped; }
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
}

extern "C" void omni_webrtc_media_router_deliver(void* context, const uint8_t* packet, size_t packet_len)
{
    auto* router = static_cast<OmniWebRtcMediaRouter*>(context);
    if (router == nullptr || packet == nullptr || packet_len == 0) {
        return;
    }
    std::lock_guard<std::mutex> lock(router->mutex);
    router->DeliverUnsafe(packet, packet_len);
}
