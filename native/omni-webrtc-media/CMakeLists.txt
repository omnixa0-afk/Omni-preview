cmake_minimum_required(VERSION 3.24)

project(omni_webrtc_media VERSION 0.1.0 LANGUAGES CXX)

# СИМВОЛЫ ДЛЯ RELEASE.
#
# Release собирался без PDB, и это стоило дорого: набор падал по SIGSEGV примерно в каждом третьем
# прогоне ИМЕННО в Release (в RelWithDebInfo десять прогонов подряд чистые — тайминги другие), а
# разобрать падение было нечем. Отладчика на машине нет, и без символов даже собственный обработчик
# печатал бы одни адреса.
#
# /Zi добавляет отладочную информацию, НЕ трогая оптимизацию (/O2 остаётся), /DEBUG кладёт PDB рядом
# с бинарником. Цена — размер файла на диске; выгода — что падение у пользователя вообще можно
# разобрать. См. docs/decisions/video-quality-ceilings-and-twcc.md, разбор #48.
if(MSVC)
    add_compile_options($<$<CONFIG:Release>:/Zi>)
    add_link_options($<$<CONFIG:Release>:/DEBUG> $<$<CONFIG:Release>:/OPT:REF> $<$<CONFIG:Release>:/OPT:ICF>)
endif()

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

option(OMNI_WEBRTC_MEDIA_ENABLE_LIBWEBRTC "Link the external libwebrtc codec probe backend" OFF)
set(OMNI_WEBRTC_MEDIA_WEBRTC_SRC "" CACHE PATH "Path to the WebRTC src checkout")
set(OMNI_WEBRTC_MEDIA_WEBRTC_OUT "" CACHE PATH "Path to the WebRTC GN output directory")

add_library(omni_webrtc_media SHARED
    src/omni_webrtc_media.cpp
)

target_include_directories(omni_webrtc_media
    PUBLIC
        $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>
)

target_compile_definitions(omni_webrtc_media
    PRIVATE
        OMNI_WEBRTC_MEDIA_BUILDING_DLL
)

if(WIN32)
    # Без NOMINMAX <windows.h> определяет макросы min/max, и любой `std::min(` в этом файле
    # разваливается с C2589 «недопустимая лексема справа от ::». В libwebrtc-ветке это уже стояло
    # (она тащит полный набор дефайнов Chromium), а в сборке без libwebrtc — нет, из-за чего
    # `scripts/build-omni-webrtc-media.ps1` БЕЗ аргументов не собирался вовсе. Определение
    # относится к платформе, а не к варианту зависимостей, поэтому его место здесь.
    target_compile_definitions(omni_webrtc_media
        PRIVATE
            NOMINMAX
            WIN32_LEAN_AND_MEAN
    )
endif()

if(OMNI_WEBRTC_MEDIA_ENABLE_LIBWEBRTC)
    if(NOT OMNI_WEBRTC_MEDIA_WEBRTC_SRC OR NOT EXISTS "${OMNI_WEBRTC_MEDIA_WEBRTC_SRC}/BUILD.gn")
        message(FATAL_ERROR "OMNI_WEBRTC_MEDIA_WEBRTC_SRC must point to a WebRTC src checkout")
    endif()

    if(NOT OMNI_WEBRTC_MEDIA_WEBRTC_OUT OR NOT EXISTS "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/webrtc.lib")
        message(FATAL_ERROR "OMNI_WEBRTC_MEDIA_WEBRTC_OUT must point to a GN output directory with obj/webrtc.lib")
    endif()

    set(OMNI_WEBRTC_MEDIA_REQUIRED_OBJECTS
        "obj/api/video_codecs/builtin_video_encoder_factory/builtin_video_encoder_factory.obj"
        "obj/api/video_codecs/builtin_video_decoder_factory/builtin_video_decoder_factory.obj"
        "obj/media/rtc_internal_video_codecs/internal_encoder_factory.obj"
        "obj/media/rtc_internal_video_codecs/internal_decoder_factory.obj"
        "obj/media/rtc_simulcast_encoder_adapter/simulcast_encoder_adapter.obj"
        "obj/api/video_codecs/rtc_software_fallback_wrappers/video_encoder_software_fallback_wrapper.obj"
        "obj/api/video_codecs/rtc_software_fallback_wrappers/video_decoder_software_fallback_wrapper.obj"
        "obj/test/fake_video_codecs/fake_encoder.obj"
        "obj/test/fake_video_codecs/fake_decoder.obj"
        "obj/test/fake_video_codecs/configurable_frame_size_encoder.obj"
        # Wave 4 Phase 2a (2026-05-16): GoogCC network controller factory.
        # Sub-modules (alr_detector, delay_based_bwe, probe_bitrate_estimator,
        # acknowledged_bitrate_estimator, send_side_bandwidth_estimation,
        # trendline_estimator, robust_throughput_estimator,
        # congestion_window_pushback_controller, loss_based_bwe_v2, etc.) are
        # pulled in via GLOB_RECURSE below — listing every .obj here would
        # be brittle across libwebrtc revisions.
        "obj/api/transport/goog_cc/goog_cc_factory.obj"
    )

    # GoogCC sub-modules — pulled recursively so we don't churn this list on
    # libwebrtc updates. If the linker reports an unresolved goog_cc symbol,
    # check whether the GN output adds a new sub-folder we need to glob.
    file(GLOB_RECURSE OMNI_WEBRTC_MEDIA_GOOGCC_OBJECTS
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/modules/congestion_controller/goog_cc/*.obj"
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/modules/congestion_controller/rtp/*.obj"
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/modules/remote_bitrate_estimator/*.obj"
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/modules/pacing/*.obj"
        # Этап 1 перехода на апстримовый RTP-отправитель (#50, ADR раздел «Этап 1 перехода»).
        # Здесь лежат ModuleRtpRtcpImpl2, RTPSender, RtpSenderVideo, RtpSenderEgress,
        # RtpPacketHistory, RTCPSender — то есть нумерация, история отправки, ответ на NACK, RTX,
        # добивка и RTCP одним куском. Всё это уже собрано GN (98 объектов), мы просто не линковали.
        #
        # Шаг НАМЕРЕННО отдельный и additive: 98 новых объектов — первое место, где перенос может
        # упереться в конфликт символов, и узнать об этом надо ДО того, как тронут путь отправки.
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/modules/rtp_rtcp/*.obj"
        # СБОРЩИК, А НЕ ДЕТАЛИ (#57). RtpTransportControllerSend + RtpVideoSender — тот самый
        # класс, который соединяет GoogCC, пейсер, роутер, адаптер отчётов и RTP-модули В ОДНО.
        # До сих пор мы брали у апстрима детали и писали соединение сами, узнавая о каждом
        # забытом проводе в момент, когда его отсутствие ломало замер. За 2026-08-23 таких
        # проводов нашлось четыре, и ещё три видно в коде.
        #
        # Шаг НАМЕРЕННО отдельный и additive: сперва узнать, что объекты линкуются и не тянут за
        # собой Call/ICE/DTLS, и только потом трогать путь отправки. Ровно так же начинался #50, и
        # именно этот шаг тогда снял главный риск заранее.
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/call/rtp_sender/*.obj"
        # ВИРТУАЛЬНЫЕ ЧАСЫ ДЛЯ СТЕНДОВ (#59). GlobalSimulatedTimeController — штатный
        # способ апстрима мерить сеть и адаптацию: время двигает тест, а не операционная
        # система, и прогон становится детерминированным. Мы весь 2026-08-24 сравнивали
        # серии, снятые с разницей в часы на загруженной машине, и получили выводы, которым
        # нельзя верить.
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/test/time_controller/*.obj"
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/api/time_controller/*.obj"
        # Из тестовых утилит нужен РОВНО ОДИН объект — подмена базовых часов, на которой
        # держится GlobalSimulatedTimeController. Целиком каталог брать нельзя: там лежат
        # серверные заглушки сокетов (virtual_socket_server, proxy_server), тянущие
        # FifoBuffer и SocketAddressPair, которых нам не нужно и которых нет в сборке.
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/rtc_base/rtc_base_tests_utils/fake_clock.obj"
    )

    # Wave 6 (2026-05-18): libwebrtc adaptation primitives. We use only the
    # low-level pair (OveruseFrameDetector + QualityScaler) directly, not the
    # full ResourceAdaptationProcessor framework (which is tightly coupled to
    # libwebrtc's VideoStreamEncoder, which we don't use).
    # See docs/decisions/webrtc-quality-scaler-adoption.md.
    #
    # Wave 5b (2026-05-18): NackRequester replaces the self-written NACK
    # tracker (~150 LOC) with libwebrtc's RTT-aware reorder + retry logic.
    # Our NackSender + KeyFrameRequestSender adapters bridge into the
    # existing OMNI_WEBRTC_MEDIA_CONTROL_NACK / _PLI event queue.
    # See docs/decisions/pacing-nack-playout-adoption.md.
    #
    # Wave 5c (2026-05-18): FrameBuffer + JitterEstimator replace the
    # self-written fixed-60ms playout deque. FrameBuffer handles bounded
    # storage + decodability tracking; JitterEstimator computes an
    # adaptive playout delay from inter-arrival jitter. Both run under
    # the session mutex_ (FrameBuffer is thread-unsafe by contract;
    # JitterEstimator likewise).
    set(OMNI_WEBRTC_MEDIA_ADAPTATION_OBJECTS
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/video/adaptation/video_adaptation/overuse_frame_detector.obj"
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/modules/video_coding/video_coding_utility/quality_scaler.obj"
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/modules/video_coding/nack_requester/nack_requester.obj"
        # ПРИЁМНАЯ ОРКЕСТРАЦИЯ ЦЕЛИКОМ АПСТРИМОВСКАЯ (#60): буфер показа, планировщик показа,
        # расчёт времени показа, сторож декодируемости и межкадровый разброс.
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/video/video_receive_stream_timeout_tracker/video_receive_stream_timeout_tracker.obj"
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/video/video_stream_buffer_controller/video_stream_buffer_controller.obj"
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/video/task_queue_frame_decode_scheduler/task_queue_frame_decode_scheduler.obj"
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/video/frame_decode_timing/frame_decode_timing.obj"
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/modules/video_coding/timing/timing_module/timing.obj"
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/modules/video_coding/timing/timestamp_extrapolator/timestamp_extrapolator.obj"
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/modules/video_coding/timing/decode_time_percentile_filter/decode_time_percentile_filter.obj"
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/modules/video_coding/timing/inter_frame_delay_variation_calculator/inter_frame_delay_variation_calculator.obj"
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/modules/video_coding/frame_helpers/frame_helpers.obj"
        # Опыты сборки. Нужны из-за одного ключа: RTCPReceiver разбирает отчёт о доставке
        # RFC 8888 только при включённом WebRTC-RFC8888CongestionControlFeedback.
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/api/field_trials/field_trials.obj"
        # RtpVideoSender разыменовывает fec_controller_ БЕЗУСЛОВНО (rtp_video_sender.cc:480),
        # то есть нулём его оставить нельзя. Апстрим в этом случае ставит FecControllerDefault
        # (call.cc:943) — берём его же. Сам FEC при этом не включается: включает его
        # RtpConfig, а у нас там ни flexfec, ни ulpfec не заданы.
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/modules/video_coding/video_coding/fec_controller_default.obj"
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/api/video/frame_buffer/frame_buffer.obj"
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/modules/video_coding/timing/jitter_estimator/jitter_estimator.obj"
        # Wave 7 (2026-08-17): битрейт, который кодировщик РЕАЛЬНО отдаёт, против того, который мы
        # у него просили. Замер этого дня: при цели 150 кбит/с аппаратный H.264 клал на провод 605,
        # программный 331 — и до сих пор этого не видел никто, потому что смотрели на просьбу, а не
        # на выход. EncoderBitrateAdjuster ведёт трёхсекундное окно фактического выхода на слой и
        # правит аллокацию, которую мы отдаём кодировщику, чтобы в цель попадал ФАКТ.
        # Нужен прежде всего аппаратному пути: у MFT нет флага пропуска кадров, и единственный
        # способ добиться от него нужной скорости — просить меньше, чем хочешь. Взято вместо
        # самописного контура (задача #38). Замер: docs/decisions/video-quality-ceilings-and-twcc.md.
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/video/video_stream_encoder_impl/encoder_bitrate_adjuster.obj"
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/video/video_stream_encoder_impl/encoder_overshoot_detector.obj"
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/video/video_stream_encoder_impl/rate_utilization_tracker.obj"
        # Таблица «какой битрейт вообще несёт такое разрешение». Не наша выдумка и не поле для
        # подбора: у H.264 1280x720 требует от 900 кбит/с на старт, а 150 кбит/с — это 320x180.
        # Именно из-за отсутствия этой связи мы весь день просили у 720p вшестеро меньше минимума
        # и считали виноватым кодировщик.
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/rtc_base/experiments/encoder_info_settings/encoder_info_settings.obj"
        # RTX на приёме (#53). Снятие обёртки RFC 4588 — двухбайтовый заголовок с исходным номером
        # — и восстановление медиапакета. Писать это руками ради двух байт значило бы завести
        # третье место, где мы знаем формат провода лучше, чем апстрим.
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/call/rtp_receiver/rtx_receive_stream.obj"
    )
    foreach(OMNI_WEBRTC_MEDIA_ADAPTATION_OBJ IN LISTS OMNI_WEBRTC_MEDIA_ADAPTATION_OBJECTS)
        if(NOT EXISTS "${OMNI_WEBRTC_MEDIA_ADAPTATION_OBJ}")
            message(FATAL_ERROR "Required WebRTC adaptation object missing: ${OMNI_WEBRTC_MEDIA_ADAPTATION_OBJ}")
        endif()
    endforeach()

    # Audio Phase 2a-step1 (2026-05-21): libopus's main static lib
    # `opus.lib` references SSE4.1 symbols (silk_NSQ_del_dec_sse4_1
    # etc.) whose definitions live in a sister GN target
    # `opus_sse41` and never make it into `opus.lib`. Pull them in
    # via GLOB so the linker can resolve them. If libwebrtc rebuilds
    # without these (e.g. SSE4.1 disabled in args.gn) the linker
    # error will reappear and direct us to update this glob.
    file(GLOB OMNI_WEBRTC_MEDIA_OPUS_SSE41_OBJECTS
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/third_party/opus/opus_sse41/*.obj"
    )
    if(NOT OMNI_WEBRTC_MEDIA_OPUS_SSE41_OBJECTS)
        message(FATAL_ERROR "WebRTC opus_sse41 objects are missing under ${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}")
    endif()

    set(OMNI_WEBRTC_MEDIA_WEBRTC_OBJECTS "")
    foreach(OMNI_WEBRTC_MEDIA_OBJECT_RELATIVE IN LISTS OMNI_WEBRTC_MEDIA_REQUIRED_OBJECTS)
        set(OMNI_WEBRTC_MEDIA_OBJECT_PATH "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/${OMNI_WEBRTC_MEDIA_OBJECT_RELATIVE}")
        if(NOT EXISTS "${OMNI_WEBRTC_MEDIA_OBJECT_PATH}")
            message(FATAL_ERROR "Required WebRTC object is missing: ${OMNI_WEBRTC_MEDIA_OBJECT_PATH}")
        endif()
        list(APPEND OMNI_WEBRTC_MEDIA_WEBRTC_OBJECTS "${OMNI_WEBRTC_MEDIA_OBJECT_PATH}")
    endforeach()

    file(GLOB OMNI_WEBRTC_MEDIA_LIBCPP_OBJECTS
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/buildtools/third_party/libc++/libc++/*.obj"
    )
    if(NOT OMNI_WEBRTC_MEDIA_LIBCPP_OBJECTS)
        message(FATAL_ERROR "WebRTC libc++ objects are missing under ${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}")
    endif()

    # Библиотека времени выполнения ВСЕГДА выпускная — даже в конфигурации Debug.
    #
    # Объектные файлы libwebrtc и его libc++ приходят из сборки GN (is_debug=false) и слинкованы с
    # выпускной CRT; NDEBUG для нашей цели ниже задан по той же причине. Если в Debug выбрать
    # отладочную CRT, в одном образе окажутся две несовместимые кучи и две разные раскладки
    # контейнеров std — память, выделенная кодом libwebrtc, будет освобождаться чужим
    # аллокатором. Проявляется это не ошибкой сборки, а падением 0xC0000005 в произвольном месте
    # (2026-08-18: разбор смоук-набора, где Debug-libwebrtc падал на первом же тесте качества, а
    # Release проходил целиком — выглядело как дефект правки, оказалось конфигурацией сборки).
    #
    # Debug здесь означает «с отладочной информацией и без оптимизаций», а не «с отладочной CRT».
    set_target_properties(omni_webrtc_media PROPERTIES
        MSVC_RUNTIME_LIBRARY "MultiThreaded"
    )

    target_compile_definitions(omni_webrtc_media
        PRIVATE
            OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC
            _HAS_NODISCARD
            _CRT_NONSTDC_NO_WARNINGS
            _WINSOCK_DEPRECATED_NO_WARNINGS
            _LIBCPP_HARDENING_MODE=_LIBCPP_HARDENING_MODE_EXTENSIVE
            _LIBCPP_DISABLE_VISIBILITY_ANNOTATIONS
            _LIBCPP_INSTRUMENTED_WITH_ASAN=0
            __STD_C
            _CRT_RAND_S
            _CRT_SECURE_NO_DEPRECATE
            _SCL_SECURE_NO_DEPRECATE
            _ATL_NO_OPENGL
            _WINDOWS
            CERT_CHAIN_PARA_HAS_EXTRA_FIELDS
            PSAPI_VERSION=2
            WIN32
            _SECURE_ATL
            WINAPI_FAMILY=WINAPI_FAMILY_DESKTOP_APP
            USE_AURA=1
            WIN32_LEAN_AND_MEAN
            NOMINMAX
            _UNICODE
            UNICODE
            NTDDI_VERSION=NTDDI_WIN11_GE
            _WIN32_WINNT=0x0A00
            WINVER=0x0A00
            WINCRYPT_USE_SYMBOL_PREFIX
            NDEBUG
            NVALGRIND
            DYNAMIC_ANNOTATIONS_ENABLED=0
            WEBRTC_ENABLE_PROTOBUF=1
            WEBRTC_STRICT_FIELD_TRIALS=0
            WEBRTC_INCLUDE_INTERNAL_AUDIO_DEVICE
            RTC_ENABLE_VP9
            RTC_ENABLE_H265
            RTC_DAV1D_IN_INTERNAL_DECODER_FACTORY
            WEBRTC_HAVE_SCTP
            WEBRTC_ENCODER_PSNR_STATS
            PROTOBUF_ENABLE_DEBUG_LOGGING_MAY_LEAK_PII=0
            WEBRTC_ENABLE_AVX2
            RTC_ENABLE_WIN_WGC
            WEBRTC_DEPRECATE_PLAN_B
            WEBRTC_NON_STATIC_TRACE_EVENT_HANDLERS=0
            WEBRTC_WIN
            ABSL_ALLOCATOR_NOTHROW=1
            CHROMIUM
            LIBYUV_DISABLE_NEON
            LIBYUV_DISABLE_SVE
            LIBYUV_DISABLE_SME
            LIBYUV_DISABLE_LSX
            LIBYUV_DISABLE_LASX
    )

    target_include_directories(omni_webrtc_media
        BEFORE PRIVATE
            "${OMNI_WEBRTC_MEDIA_WEBRTC_SRC}"
            "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/gen"
            "${OMNI_WEBRTC_MEDIA_WEBRTC_SRC}/buildtools/third_party/libc++"
            "${OMNI_WEBRTC_MEDIA_WEBRTC_SRC}/third_party/libc++/src/include"
            "${OMNI_WEBRTC_MEDIA_WEBRTC_SRC}/third_party/abseil-cpp"
            "${OMNI_WEBRTC_MEDIA_WEBRTC_SRC}/third_party/perfetto/include"
            "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/gen/third_party/perfetto/build_config"
            "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/gen/third_party/perfetto"
            "${OMNI_WEBRTC_MEDIA_WEBRTC_SRC}/third_party/libyuv/include"
            "${OMNI_WEBRTC_MEDIA_WEBRTC_SRC}/third_party/protobuf/src"
            # Audio Phase 2a (2026-05-21): libopus headers for opus
            # encode/decode. The static lib itself is linked below.
            # See docs/decisions/audio-phase-1-adoption.md.
            "${OMNI_WEBRTC_MEDIA_WEBRTC_SRC}/third_party/opus/src/include"
    )

    target_link_libraries(omni_webrtc_media
        PRIVATE
            ${OMNI_WEBRTC_MEDIA_WEBRTC_OBJECTS}
            ${OMNI_WEBRTC_MEDIA_GOOGCC_OBJECTS}
            ${OMNI_WEBRTC_MEDIA_ADAPTATION_OBJECTS}
            "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/webrtc.lib"
            # Audio Phase 2a (2026-05-21): libopus static lib pre-built
            # by libwebrtc's own toolchain. opus.lib references SSE4.1
            # symbols (silk_NSQ_del_dec_sse4_1 etc.) defined in
            # opus_sse41/ obj files that are NOT in opus.lib itself —
            # libwebrtc's GN graph keeps them as a separate sub-target.
            # Pull them in via GLOB. See
            # docs/decisions/audio-phase-1-adoption.md.
            "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/third_party/opus/opus.lib"
            ${OMNI_WEBRTC_MEDIA_OPUS_SSE41_OBJECTS}
            ${OMNI_WEBRTC_MEDIA_LIBCPP_OBJECTS}
            "${OMNI_WEBRTC_MEDIA_WEBRTC_SRC}/third_party/llvm-build/Release+Asserts/lib/clang/23/lib/windows/clang_rt.builtins-x86_64.lib"
            libcpmt.lib
            libconcrt.lib
            advapi32.lib
            bcrypt.lib
            crypt32.lib
            d3d11.lib
            dmoguids.lib
            dxgi.lib
            gdi32.lib
            iphlpapi.lib
            mf.lib
            mfplat.lib
            mfuuid.lib
            msdmo.lib
            ole32.lib
            oleaut32.lib
            runtimeobject.lib
            secur32.lib
            shlwapi.lib
            strmiids.lib
            user32.lib
            uuid.lib
            winmm.lib
            ws2_32.lib
    )

    target_sources(omni_webrtc_media
        PRIVATE
            "${OMNI_WEBRTC_MEDIA_WEBRTC_SRC}/api/video/rtp_video_frame_assembler.cc"
    )
endif()

if(WIN32)
    # D3D11 video renderer (SwapChainPanel path) links the OS GPU stack in BOTH the
    # libwebrtc and stub builds. Vendor-neutral — never NVENC/AMF/QSV.
    target_link_libraries(omni_webrtc_media PRIVATE d3d11 dxgi d3dcompiler)
endif()

if(MSVC)
    target_compile_options(omni_webrtc_media PRIVATE /W4 /WX /permissive-)
    if(OMNI_WEBRTC_MEDIA_ENABLE_LIBWEBRTC AND CMAKE_CXX_COMPILER_ID MATCHES "Clang")
        target_compile_options(omni_webrtc_media PRIVATE
            -Wno-nullability-completeness
            -Wno-unsafe-buffer-usage
            -Wno-unused-parameter
        )
    endif()
else()
    target_compile_options(omni_webrtc_media PRIVATE -Wall -Wextra -Wpedantic -Werror)
endif()

# Reproducible Release binaries: the DLL names its PDB by file name only, and source paths in
# assertion text — ours and the libwebrtc sources compiled in — are mapped to neutral prefixes, so the
# output does not depend on where the tree lives. Debug keeps real paths so the debugger finds the sources.
if(MSVC)
    target_link_options(omni_webrtc_media PRIVATE "/PDBALTPATH:%_PDB%")
    if(CMAKE_CXX_COMPILER_ID MATCHES "Clang")
        set(_omni_prefix_maps "${CMAKE_CURRENT_SOURCE_DIR}=omni-webrtc-media")
        if(OMNI_WEBRTC_MEDIA_WEBRTC_SRC)
            list(APPEND _omni_prefix_maps "${OMNI_WEBRTC_MEDIA_WEBRTC_SRC}=webrtc")
        endif()
        foreach(_map IN LISTS _omni_prefix_maps)
            string(REPLACE "=" ";" _parts "${_map}")
            list(GET _parts 0 _from)
            list(GET _parts 1 _to)
            file(TO_NATIVE_PATH "${_from}" _from_native)
            # Both spellings: CMake passes forward slashes, the preprocessor may record backslashes.
            target_compile_options(omni_webrtc_media PRIVATE
                "$<$<NOT:$<CONFIG:Debug>>:/clang:-ffile-prefix-map=${_from}=${_to}>"
                "$<$<NOT:$<CONFIG:Debug>>:/clang:-ffile-prefix-map=${_from_native}=${_to}>")
        endforeach()
    endif()
endif()

add_executable(omni_webrtc_media_smoke
    tests/omni_webrtc_media_smoke.cpp
)

target_link_libraries(omni_webrtc_media_smoke PRIVATE omni_webrtc_media)
if(WIN32)
    # dbghelp — печать стека при падении набора (#48). Отладчика на машине нет (cdb не
    # установлен), поэтому раскрутку и символизацию берём у самой ОС, а не пишем свою.
    # Диагностическая обвязка ТОЛЬКО тестового бинарника: в omni_webrtc_media её нет.
    target_link_libraries(omni_webrtc_media_smoke PRIVATE dbghelp)
endif()

if(OMNI_WEBRTC_MEDIA_ENABLE_LIBWEBRTC)
    target_compile_definitions(omni_webrtc_media_smoke PRIVATE OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    # Audio Phase 2a-step1 (2026-05-21): smoke directly exercises the
    # libopus API (opus_encode/decode etc.). The library target's
    # PRIVATE includes / links don't propagate through
    # target_link_libraries(omni_webrtc_media_smoke ... omni_webrtc_media),
    # and the shared lib does not re-export libopus symbols. We
    # replicate both — include path + static lib — on the smoke
    # target so the test sees the same opus surface the production
    # build will use.
    target_include_directories(omni_webrtc_media_smoke PRIVATE
        "${OMNI_WEBRTC_MEDIA_WEBRTC_SRC}/third_party/opus/src/include"
    )
    target_link_libraries(omni_webrtc_media_smoke PRIVATE
        "${OMNI_WEBRTC_MEDIA_WEBRTC_OUT}/obj/third_party/opus/opus.lib"
        ${OMNI_WEBRTC_MEDIA_OPUS_SSE41_OBJECTS}
    )
    # Та же выпускная CRT, что и у библиотеки: смоук линкует объекты из той же сборки GN, и
    # разойтись им нельзя (см. пояснение у MSVC_RUNTIME_LIBRARY выше).
    set_target_properties(omni_webrtc_media_smoke PROPERTIES
        MSVC_RUNTIME_LIBRARY "MultiThreaded"
    )
endif()

enable_testing()
add_test(NAME omni_webrtc_media_smoke COMMAND omni_webrtc_media_smoke)
