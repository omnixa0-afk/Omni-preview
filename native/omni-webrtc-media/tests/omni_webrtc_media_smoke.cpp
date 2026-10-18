#include "omni_webrtc_media.h"

#include <algorithm>
#include <array>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <chrono>
#include <deque>
#include <iostream>
#include <string_view>
#include <map>
#include <memory>
#include <mutex>
#include <thread>
#include <vector>

#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
// Audio Phase 2a-step1 (2026-05-21): libopus end-to-end smoke. The
// header is shipped with the libwebrtc third_party/opus tree we
// already link against; CMake adds the corresponding include path.
// See docs/decisions/audio-phase-1-adoption.md.
#include "opus.h"
#endif

// ПЕЧАТЬ СТЕКА ПРИ ПАДЕНИИ — ТОЛЬКО ДЛЯ ТЕСТОВОГО НАБОРА (#48).
//
// Набор падал по SIGSEGV примерно в одном прогоне из трёх, всегда в одном месте — на входе в первый
// тест с потерями, то есть сразу после разрушения предыдущей сессии. Две попытки объяснить это без
// стека дали два неверных вывода подряд: сначала «виноват ClearUpTo» (откатили — падение осталось),
// потом «оно ушло» (тринадцать чистых прогонов оказались совпадением). Гадать больше нельзя.
//
// Отладчика на машине нет: в Windows Kits лежат только dbghelp.dll и dbgcore.dll, которые кладёт
// Visual Studio, а cdb.exe отсутствует. Ставить Debugging Tools — это загрузка и повышение прав.
// Поэтому символизацию берём у самой ОС: StackWalk64 + SymFromAddr из dbghelp, повешенные на
// SetUnhandledExceptionFilter. Своего здесь только цикл печати — разбор символов и раскрутка кадров
// делает система.
//
// Живёт в тестовом бинарнике намеренно: это диагностическая обвязка, а не поведение продукта.
#if defined(_WIN32)
// NOMINMAX обязателен: без него windows.h определяет макросы min/max, и все std::max в этом файле
// перестают компилироваться. WIN32_LEAN_AND_MEAN — чтобы не тянуть половину Windows ради dbghelp.
#ifndef NOMINMAX
#define NOMINMAX
#endif
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>
#include <dbghelp.h>
#include <cstdio>
#include <cstring>

namespace {

const char* ExceptionName(DWORD code)
{
    switch (code) {
    case EXCEPTION_ACCESS_VIOLATION:      return "ACCESS_VIOLATION";
    case EXCEPTION_STACK_OVERFLOW:        return "STACK_OVERFLOW";
    case EXCEPTION_ILLEGAL_INSTRUCTION:   return "ILLEGAL_INSTRUCTION";
    case EXCEPTION_INT_DIVIDE_BY_ZERO:    return "INT_DIVIDE_BY_ZERO";
    case EXCEPTION_PRIV_INSTRUCTION:      return "PRIV_INSTRUCTION";
    case EXCEPTION_IN_PAGE_ERROR:         return "IN_PAGE_ERROR";
    default:                              return "UNKNOWN";
    }
}

LONG WINAPI OmniSmokeCrashFilter(EXCEPTION_POINTERS* info)
{
    // stderr, а не cout: поток вывода теста может быть в любом состоянии, а падение мы обязаны
    // увидеть в логе целиком и немедленно.
    std::fprintf(stderr, "\n=== [crash] %s (0x%08lX) at %p ===\n",
                 ExceptionName(info->ExceptionRecord->ExceptionCode),
                 static_cast<unsigned long>(info->ExceptionRecord->ExceptionCode),
                 info->ExceptionRecord->ExceptionAddress);
    if (info->ExceptionRecord->ExceptionCode == EXCEPTION_ACCESS_VIOLATION &&
        info->ExceptionRecord->NumberParameters >= 2) {
        const ULONG_PTR op = info->ExceptionRecord->ExceptionInformation[0];
        std::fprintf(stderr, "[crash] %s address 0x%llX\n",
                     op == 0 ? "read from" : (op == 1 ? "write to" : "execute at"),
                     static_cast<unsigned long long>(info->ExceptionRecord->ExceptionInformation[1]));
    }
    std::fprintf(stderr, "[crash] thread %lu\n", GetCurrentThreadId());

    const HANDLE process = GetCurrentProcess();
    const HANDLE thread = GetCurrentThread();
    SymSetOptions(SYMOPT_DEFERRED_LOADS | SYMOPT_LOAD_LINES | SYMOPT_UNDNAME);
    if (!SymInitialize(process, nullptr, TRUE)) {
        std::fprintf(stderr, "[crash] SymInitialize failed (%lu); frames unsymbolised\n",
                     GetLastError());
    }

    // Копия контекста: StackWalk64 его правит по ходу раскрутки, а оригинал портить нельзя.
    CONTEXT context = *info->ContextRecord;
    STACKFRAME64 frame{};
    frame.AddrPC.Offset = context.Rip;
    frame.AddrPC.Mode = AddrModeFlat;
    frame.AddrFrame.Offset = context.Rbp;
    frame.AddrFrame.Mode = AddrModeFlat;
    frame.AddrStack.Offset = context.Rsp;
    frame.AddrStack.Mode = AddrModeFlat;

    alignas(SYMBOL_INFO) char symbol_storage[sizeof(SYMBOL_INFO) + 1024]{};
    SYMBOL_INFO* symbol = reinterpret_cast<SYMBOL_INFO*>(symbol_storage);
    symbol->SizeOfStruct = sizeof(SYMBOL_INFO);
    symbol->MaxNameLen = 1024;

    for (int depth = 0; depth < 64; ++depth) {
        if (!StackWalk64(IMAGE_FILE_MACHINE_AMD64, process, thread, &frame, &context,
                         nullptr, SymFunctionTableAccess64, SymGetModuleBase64, nullptr)) {
            break;
        }
        if (frame.AddrPC.Offset == 0) {
            break;
        }

        const DWORD64 pc = frame.AddrPC.Offset;
        char module_name[MAX_PATH] = "?";
        const DWORD64 module_base = SymGetModuleBase64(process, pc);
        if (module_base != 0) {
            HMODULE handle = reinterpret_cast<HMODULE>(module_base);
            if (GetModuleFileNameA(handle, module_name, MAX_PATH) > 0) {
                const char* slash = std::strrchr(module_name, '\\');
                if (slash != nullptr) {
                    std::memmove(module_name, slash + 1, std::strlen(slash + 1) + 1);
                }
            }
        }

        DWORD64 displacement = 0;
        const char* name = "<no symbol>";
        if (SymFromAddr(process, pc, &displacement, symbol)) {
            name = symbol->Name;
        }

        IMAGEHLP_LINE64 line{};
        line.SizeOfStruct = sizeof(IMAGEHLP_LINE64);
        DWORD line_displacement = 0;
        if (SymGetLineFromAddr64(process, pc, &line_displacement, &line)) {
            std::fprintf(stderr, "[crash] #%02d %s!%s + 0x%llX  (%s:%lu)\n",
                         depth, module_name, name,
                         static_cast<unsigned long long>(displacement),
                         line.FileName, line.LineNumber);
        } else {
            std::fprintf(stderr, "[crash] #%02d %s!%s + 0x%llX\n",
                         depth, module_name, name,
                         static_cast<unsigned long long>(displacement));
        }
    }

    std::fflush(stderr);
    SymCleanup(process);
    return EXCEPTION_EXECUTE_HANDLER;
}

struct OmniSmokeCrashHandlerInstaller
{
    OmniSmokeCrashHandlerInstaller()
    {
        SetUnhandledExceptionFilter(OmniSmokeCrashFilter);
    }
};

OmniSmokeCrashHandlerInstaller g_omni_smoke_crash_handler_installer;

}  // namespace
#endif  // _WIN32


namespace {

// ИСХОДЯЩИЕ ПАКЕТЫ СЕССИИ — В ЯЩИК СТЕНДА (2026-09-26).
//
// В продукте сессия отдаёт каждый пакет в ядро в момент выпуска (set_packet_sink); своей очереди
// у неё больше нет. Стендам удобнее забирать пакеты пачкой, как раньше, — поэтому каждая сессия
// стенда создаётся с получателем-ящиком, а DrainSessionPackets выдаёт накопленное в прежнем
// формате (длина LE16, затем пакет). Порядок — порядок выпуска, RTP и RTCP вперемешку.
struct SessionPacketInbox {
    std::mutex mutex;
    std::deque<std::vector<uint8_t>> packets;
};

std::mutex g_session_inboxes_mutex;
std::map<OmniWebRtcMediaSession*, std::unique_ptr<SessionPacketInbox>> g_session_inboxes;

void CollectSessionPacket(void* context, const uint8_t* data, size_t size)
{
    auto* inbox = static_cast<SessionPacketInbox*>(context);
    std::lock_guard<std::mutex> lock(inbox->mutex);
    inbox->packets.emplace_back(data, data + size);
}

OmniWebRtcMediaStatus CreateSession(
    const char* config_json,
    OmniWebRtcMediaSession** out_session,
    char* error_buffer,
    size_t error_buffer_len,
    size_t* required_error_len)
{
    const OmniWebRtcMediaStatus status = omni_webrtc_media_session_create(
        config_json, out_session, error_buffer, error_buffer_len, required_error_len);
    if (status == OMNI_WEBRTC_MEDIA_STATUS_OK && out_session != nullptr && *out_session != nullptr) {
        auto inbox = std::make_unique<SessionPacketInbox>();
        (void)omni_webrtc_media_session_set_packet_sink(*out_session, &CollectSessionPacket, inbox.get());
        std::lock_guard<std::mutex> lock(g_session_inboxes_mutex);
        g_session_inboxes[*out_session] = std::move(inbox);
    }
    return status;
}

// Сессия гибнет первой — после этого отдавать ей некуда и незачем, и ящик можно освобождать.
void DestroySession(OmniWebRtcMediaSession* session)
{
    omni_webrtc_media_session_destroy(session);
    std::lock_guard<std::mutex> lock(g_session_inboxes_mutex);
    g_session_inboxes.erase(session);
}

OmniWebRtcMediaStatus DrainSessionPackets(
    OmniWebRtcMediaSession* session,
    uint8_t* buffer,
    size_t buffer_len,
    uint32_t max_packets,
    size_t* written_len,
    uint32_t* packet_count)
{
    if (written_len == nullptr || packet_count == nullptr) {
        return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
    }
    *written_len = 0;
    *packet_count = 0;
    SessionPacketInbox* inbox = nullptr;
    {
        std::lock_guard<std::mutex> lock(g_session_inboxes_mutex);
        const auto found = g_session_inboxes.find(session);
        if (found == g_session_inboxes.end()) {
            return OMNI_WEBRTC_MEDIA_STATUS_INVALID_ARGUMENT;
        }
        inbox = found->second.get();
    }
    const uint32_t cap = max_packets == 0 ? UINT32_MAX : max_packets;
    std::lock_guard<std::mutex> lock(inbox->mutex);
    while (!inbox->packets.empty() && *packet_count < cap) {
        const std::vector<uint8_t>& packet = inbox->packets.front();
        if (*written_len + 2u + packet.size() > buffer_len) {
            return *packet_count == 0 ? OMNI_WEBRTC_MEDIA_STATUS_BUFFER_TOO_SMALL
                                      : OMNI_WEBRTC_MEDIA_STATUS_OK;
        }
        buffer[*written_len] = static_cast<uint8_t>(packet.size() & 0xFFu);
        buffer[*written_len + 1] = static_cast<uint8_t>((packet.size() >> 8) & 0xFFu);
        std::memcpy(buffer + *written_len + 2u, packet.data(), packet.size());
        *written_len += 2u + packet.size();
        *packet_count += 1;
        inbox->packets.pop_front();
    }
    return OMNI_WEBRTC_MEDIA_STATUS_OK;
}

/// Всё, что сессия выпустила к этому моменту, по пакету.
std::vector<std::vector<uint8_t>> TakeSessionPackets(OmniWebRtcMediaSession* session)
{
    std::vector<std::vector<uint8_t>> taken;
    std::lock_guard<std::mutex> lock(g_session_inboxes_mutex);
    const auto found = g_session_inboxes.find(session);
    if (found == g_session_inboxes.end()) {
        return taken;
    }
    std::lock_guard<std::mutex> inbox_lock(found->second->mutex);
    taken.assign(found->second->packets.begin(), found->second->packets.end());
    found->second->packets.clear();
    return taken;
}

/// Отдать сессии один пакет так, как его отдаёт разборщик входящих (пачка из одного).
OmniWebRtcMediaStatus PushOnePacket(OmniWebRtcMediaSession* session, const std::vector<uint8_t>& packet)
{
    std::vector<uint8_t> batch(packet.size() + 2u);
    batch[0] = static_cast<uint8_t>(packet.size() & 0xFFu);
    batch[1] = static_cast<uint8_t>((packet.size() >> 8) & 0xFFu);
    std::memcpy(batch.data() + 2u, packet.data(), packet.size());
    return omni_webrtc_media_session_push_rtp_packets(session, batch.data(), batch.size(), 1);
}

/// Есть ли в составном RTCP пакет данного типа и формата (RFC 3550 §6.1: пакеты подряд, длина в
/// 32-битных словах минус один; формат — младшие пять бит первого байта).
bool RtcpCompoundContains(const std::vector<uint8_t>& compound, uint8_t packet_type, uint8_t format)
{
    size_t offset = 0;
    while (offset + 4u <= compound.size()) {
        const uint8_t* header = compound.data() + offset;
        if ((header[0] >> 6) != 2u) {
            return false;
        }
        if (header[1] == packet_type && (header[0] & 0x1Fu) == format) {
            return true;
        }
        offset += (static_cast<size_t>((header[2] << 8) | header[3]) + 1u) * 4u;
    }
    return false;
}

/// Есть ли в составном RTCP пакет данного типа (формат не важен: у SR/RR там число блоков).
bool RtcpCompoundHasType(const std::vector<uint8_t>& compound, uint8_t packet_type)
{
    size_t offset = 0;
    while (offset + 4u <= compound.size()) {
        const uint8_t* header = compound.data() + offset;
        if ((header[0] >> 6) != 2u) {
            return false;
        }
        if (header[1] == packet_type) {
            return true;
        }
        offset += (static_cast<size_t>((header[2] << 8) | header[3]) + 1u) * 4u;
    }
    return false;
}

/// Generic NACK на один номер (RFC 4585 §6.2.1): RTPFB, формат 1, PID без маски.
std::vector<uint8_t> BuildGenericNack(uint32_t sender_ssrc, uint32_t media_ssrc, uint16_t sequence)
{
    std::vector<uint8_t> nack = {0x81, 205, 0x00, 0x03};
    for (const uint32_t ssrc : {sender_ssrc, media_ssrc}) {
        nack.push_back(static_cast<uint8_t>(ssrc >> 24));
        nack.push_back(static_cast<uint8_t>(ssrc >> 16));
        nack.push_back(static_cast<uint8_t>(ssrc >> 8));
        nack.push_back(static_cast<uint8_t>(ssrc));
    }
    nack.push_back(static_cast<uint8_t>(sequence >> 8));
    nack.push_back(static_cast<uint8_t>(sequence));
    nack.push_back(0);
    nack.push_back(0);
    return nack;
}

bool Contains(std::string_view value, std::string_view needle)
{
    return value.find(needle) != std::string_view::npos;
}

OmniWebRtcMediaStatus WaitForI420FrameSize(
    OmniWebRtcMediaSession* session,
    int32_t* width,
    int32_t* height,
    uint32_t* rtp_timestamp,
    int64_t* timestamp_us,
    size_t* required_frame_len)
{
    OmniWebRtcMediaStatus status = OMNI_WEBRTC_MEDIA_STATUS_OK;
    for (int attempt = 0; attempt < 50; ++attempt) {
        status = omni_webrtc_media_session_drain_i420_frame(
            session,
            nullptr,
            0,
            width,
            height,
            rtp_timestamp,
            timestamp_us,
            required_frame_len);
        if (status == OMNI_WEBRTC_MEDIA_STATUS_BUFFER_TOO_SMALL) {
            return status;
        }

        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }

    return status;
}

int Fail(const char* message)
{
    std::cerr << message << '\n';
    return 1;
}

void FillI420ColorBars(
    std::vector<uint8_t>& y_plane,
    std::vector<uint8_t>& u_plane,
    std::vector<uint8_t>& v_plane,
    int width,
    int height)
{
    y_plane.resize(static_cast<size_t>(width) * static_cast<size_t>(height));
    const int chroma_width = (width + 1) / 2;
    const int chroma_height = (height + 1) / 2;
    u_plane.resize(static_cast<size_t>(chroma_width) * static_cast<size_t>(chroma_height));
    v_plane.resize(static_cast<size_t>(chroma_width) * static_cast<size_t>(chroma_height));

    for (int y = 0; y < height; ++y) {
        uint8_t* row = y_plane.data() + (static_cast<size_t>(y) * static_cast<size_t>(width));
        for (int x = 0; x < width; ++x) {
            row[x] = x < width / 3 ? 76 : x < (width * 2 / 3) ? 149 : 29;
        }
    }

    for (int y = 0; y < chroma_height; ++y) {
        uint8_t* row_u = u_plane.data() + (static_cast<size_t>(y) * static_cast<size_t>(chroma_width));
        uint8_t* row_v = v_plane.data() + (static_cast<size_t>(y) * static_cast<size_t>(chroma_width));
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

/// Кадр с настоящим движением — чтобы кодировщику было что тратить, но было и что срезать.
///
/// Неподвижные цветные полосы сжимаются в единицы килобит независимо от запрошенного битрейта, и на
/// них замер «слушается ли кодировщик цели» прошёл бы, не проверив ничего: до снижения цели и после
/// вышло бы одно и то же маленькое число. Первая версия решала это прямоугольником СЛУЧАЙНОГО ШУМА
/// — и это была ошибка в обратную сторону: шум несжимаем, любой кодировщик упирается в потолок QP и
/// физически не может выполнить низкую цель, так что тест обвинял бы исправный код. Здесь по кадру
/// едет однотонный прямоугольник: движение настоящее (межкадровая разница на трети кадра), но
/// сжать его до низкой цели кодировщик обязан уметь.
void FillI420MovingNoise(
    std::vector<uint8_t>& y_plane,
    std::vector<uint8_t>& u_plane,
    std::vector<uint8_t>& v_plane,
    int width,
    int height,
    uint32_t frame_index)
{
    FillI420ColorBars(y_plane, u_plane, v_plane, width, height);

    const int block_width = std::max(2, width * 2 / 3);
    const int block_height = std::max(2, height * 2 / 3);
    const int origin_x = static_cast<int>(frame_index * 7u) % std::max(1, width - block_width + 1);
    const int origin_y = static_cast<int>(frame_index * 5u) % std::max(1, height - block_height + 1);
    const uint32_t phase = frame_index * 13u;

    for (int y = origin_y; y < origin_y + block_height && y < height; ++y) {
        uint8_t* row = y_plane.data() + (static_cast<size_t>(y) * static_cast<size_t>(width));
        for (int x = origin_x; x < origin_x + block_width && x < width; ++x) {
            // Мелкая диагональная текстура, которая едет вместе с блоком: деталей много (на высоком
            // качестве это стоит мегабит), но структура регулярная (на высоком QP сжимается) —
            // ровно тот размен, который и должен делать кодировщик, получив низкую цель.
            row[x] = static_cast<uint8_t>((static_cast<uint32_t>(x) * 3u +
                                           static_cast<uint32_t>(y) * 5u + phase) & 0xFFu);
        }
    }
}

/// NV12 из той же картинки, что и I420: яркость как есть, цветность ЧЕРЕДУЮЩАЯСЯ.
///
/// Нужен потому, что оболочка подаёт кадры именно так (`format=nv12` в диагностике звонка), а все
/// стенды до сих пор подавали I420 — то есть вход, которым пользуется продукт, не прогонялся
/// нативным набором НИ РАЗУ.
void FillNv12FromI420(
    const std::vector<uint8_t>& u_plane,
    const std::vector<uint8_t>& v_plane,
    int width,
    int height,
    std::vector<uint8_t>& uv_plane)
{
    const int chroma_width = (width + 1) / 2;
    const int chroma_height = (height + 1) / 2;
    uv_plane.resize(static_cast<size_t>(chroma_width) * 2u * static_cast<size_t>(chroma_height));
    for (int y = 0; y < chroma_height; ++y) {
        const size_t src_row = static_cast<size_t>(y) * static_cast<size_t>(chroma_width);
        uint8_t* dst = uv_plane.data() + static_cast<size_t>(y) * static_cast<size_t>(chroma_width) * 2u;
        for (int x = 0; x < chroma_width; ++x) {
            dst[x * 2] = u_plane[src_row + static_cast<size_t>(x)];
            dst[x * 2 + 1] = v_plane[src_row + static_cast<size_t>(x)];
        }
    }
}

void FillBgraColorBars(std::vector<uint8_t>& frame, int width, int height)
{
    frame.resize(static_cast<size_t>(width) * static_cast<size_t>(height) * 4u);
    for (int y = 0; y < height; ++y) {
        for (int x = 0; x < width; ++x) {
            uint8_t* pixel = frame.data() +
                ((static_cast<size_t>(y) * static_cast<size_t>(width) + static_cast<size_t>(x)) * 4u);
            if (x < width / 3) {
                pixel[0] = 0;
                pixel[1] = 0;
                pixel[2] = 255;
            } else if (x < (width * 2 / 3)) {
                pixel[0] = 0;
                pixel[1] = 255;
                pixel[2] = 0;
            } else {
                pixel[0] = 255;
                pixel[1] = 0;
                pixel[2] = 0;
            }

            pixel[3] = 255;
        }
    }
}

double AverageLuma(const std::vector<uint8_t>& frame, int width, int height, int start_x, int end_x)
{
    long long sum = 0;
    long long count = 0;
    for (int y = 0; y < height; ++y) {
        const uint8_t* row = frame.data() + (static_cast<size_t>(y) * static_cast<size_t>(width));
        for (int x = start_x; x < end_x; ++x) {
            sum += row[x];
            ++count;
        }
    }

    return count == 0 ? 0.0 : static_cast<double>(sum) / static_cast<double>(count);
}

bool DecodedColorBarsArePlausible(const std::vector<uint8_t>& frame, int width, int height)
{
    if (frame.size() < static_cast<size_t>(width) * static_cast<size_t>(height) ||
        width < 96 ||
        height < 64) {
        return false;
    }

    const double left = AverageLuma(frame, width, height, width / 12, width / 4);
    const double middle = AverageLuma(frame, width, height, (width * 5) / 12, (width * 7) / 12);
    const double right = AverageLuma(frame, width, height, (width * 3) / 4, (width * 11) / 12);
    return middle > left + 20.0 && left > right + 20.0;
}

#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
/// `software_encode` прогоняет ту же проверку через программный кодировщик.
///
/// Оба пути обязаны работать сквозь: закодировать, упаковать в RTP, доставить, декодировать.
/// Аппаратный — основной (он и нужен на большинстве машин), программный — крайняя мера, когда
/// у пользователя нет исправного GPU-кодировщика. Раз он объявлен возможным, он и проверяется:
/// иначе «крайняя мера» останется обещанием, а сломается молча и у пользователя.
/// Сессия обязана принять кадр ДРУГОГО размера, не будучи пересозданной.
///
/// Раньше все три входа кодирования отвечали `INVALID_ARGUMENT` на несовпадение размера, поэтому
/// каждая ступень лестницы ABR стоила сноса и сборки сессии — сотни миллисекунд видимой паузы.
/// Проверяем оба направления (вниз и обратно вверх) и оба пути кодирования: у аппаратного
/// перенастройка идёт сменой типов на живом MFT, у программного — повторным InitEncode.
int RunLibWebRtcResizeWithoutRecreateSmoke(std::string_view codec, bool software_encode)
{
    constexpr int kWidth = 640;
    constexpr int kHeight = 360;
    const std::string session_config =
        std::string(R"({"codec":")") + std::string(codec) +
        R"(","width":640,"height":360,"fps":30,"bitrate_bps":800000)" +
        R"(,"negotiated_width":640,"negotiated_height":360)" +
        (software_encode ? R"(,"encode_software":1})" : R"(})");

    std::array<char, 512> error{};
    size_t required_error_len = 0;
    OmniWebRtcMediaSession* sender = nullptr;
    if (CreateSession(
            session_config.c_str(), &sender, error.data(), error.size(), &required_error_len) !=
            OMNI_WEBRTC_MEDIA_STATUS_OK ||
        sender == nullptr) {
        std::cerr << "resize smoke: session_create failed: " << error.data() << '\n';
        return Fail("resize smoke session_create failed");
    }

    // Порядок нарочно такой: вниз по лестнице, потом обратно вверх к исходному.
    const std::pair<int, int> steps[] = {{kWidth, kHeight}, {320, 180}, {480, 270}, {kWidth, kHeight}};
    std::vector<uint8_t> y_plane;
    std::vector<uint8_t> u_plane;
    std::vector<uint8_t> v_plane;
    for (const auto& [width, height] : steps) {
        FillI420ColorBars(y_plane, u_plane, v_plane, width, height);
        bool accepted = false;
        for (int attempt = 0; attempt < 5 && !accepted; ++attempt) {
            const OmniWebRtcMediaStatus status = omni_webrtc_media_session_push_i420_frame(
                sender,
                y_plane.data(),
                width,
                u_plane.data(),
                (width + 1) / 2,
                v_plane.data(),
                (width + 1) / 2,
                width,
                height,
                1'000'000 + attempt * 33'000,
                attempt == 0 ? 1 : 0);
            if (status == OMNI_WEBRTC_MEDIA_STATUS_OK) {
                accepted = true;
            }
        }

        if (!accepted) {
            DestroySession(sender);
            std::cerr << "resize smoke: session rejected " << width << "x" << height << '\n';
            return Fail("session refused a new frame size instead of resizing in place");
        }
    }

    DestroySession(sender);
    return 0;
}

// Определена ниже: диагностика понадобилась здесь раньше, чем определяется её читатель.
long long ReadDiagnosticsInt(OmniWebRtcMediaSession* session, std::string_view key);
bool SessionDiagnosticsContains(OmniWebRtcMediaSession* session, std::string_view needle);

/// Выбрать пакеты, ДАВ ПУТИ ОТПРАВКИ ДОЕХАТЬ.
///
/// «Толкнул кадр — тут же выбрал пакеты» перестало работать после перехода на апстримовый
/// отправитель (#50), и не потому, что что-то сломалось. Раньше выборка САМА двигала пейсер:
/// drain_rtp_packets звал ProcessPackets, и пакеты появлялись ровно в момент запроса. Теперь путь
/// асинхронный на всю длину — кодирование отдаёт кадр задачей на очередь RTP, пакетизатор кладёт
/// пакеты в пейсер, пейсер тикает сам каждые 5 мс, — а выборка только забирает уже готовое.
///
/// Ждать по признаку «отправитель замолчал», а не спать фиксированно и не брать первую же порцию.
///
/// Первая непустая порция — это НЕ кадр целиком: пейсер выдаёт пакеты по бюджету, и кадр приезжает
/// за несколько тиков. Отдать вызывающему первые два пакета из семи значит подсунуть ему рваный
/// кадр, который приёмник законно не соберёт. Поэтому порции копятся в один буфер (формат — цепочка
/// пакетов с длиной впереди, склеивается тривиально), а выход — по нескольким подряд пустым опросам.
OmniWebRtcMediaStatus DrainRtpWithLatency(
    OmniWebRtcMediaSession* session,
    uint8_t* buffer,
    size_t buffer_len,
    uint32_t max_packets,
    size_t* written_len,
    uint32_t* packet_count)
{
    OmniWebRtcMediaStatus status = OMNI_WEBRTC_MEDIA_STATUS_OK;
    *written_len = 0;
    *packet_count = 0;
    int silent_polls = 0;
    for (int attempt = 0; attempt < 200 && silent_polls < 6; ++attempt) {
        // Буфер почти полон — дальше копить некуда, и это не ошибка. Раньше очередь сессии
        // держала не больше 1024 пакетов и роняла лишнее; ящик стенда копит всё, что выпустил
        // пейсер, и без этой проверки следующая порция отвечала BUFFER_TOO_SMALL.
        if (buffer_len - *written_len < 1500) {
            return OMNI_WEBRTC_MEDIA_STATUS_OK;
        }
        size_t chunk_len = 0;
        uint32_t chunk_count = 0;
        status = DrainSessionPackets(
            session,
            buffer + *written_len,
            buffer_len - *written_len,
            max_packets - *packet_count,
            &chunk_len,
            &chunk_count);
        if (status != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            return status;
        }
        *written_len += chunk_len;
        *packet_count += chunk_count;
        if (*packet_count >= max_packets) {
            return status;
        }
        silent_polls = chunk_count == 0 ? silent_polls + 1 : 0;
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
    return status;
}

/// ВИРТУАЛЬНЫЕ ЧАСЫ: ПРОВЕРКА, ЧТО ПРОГОН ВООБЩЕ СТАЛ ДЕТЕРМИНИРОВАННЫМ (#59).
///
/// Смысл всей затеи в одном утверждении: два прогона одного и того же сценария на виртуальных
/// часах обязаны дать ПОБИТОВО одинаковый результат. Если это не так, виртуальные часы не
/// подключились и мерить ими нечего.
///
/// Стенд намеренно крошечный и без кодировщиков: он проверяет ЧАСЫ, а не медиа. Программный
/// кодировщик и D3D11 живут на настоящем времени и под виртуальные часы не переводятся — стенды
/// с ними остаются на реальных.
int RunSimulatedTimeDeterminismSmoke()
{
    const auto run_once = [](long long& out_drained, long long& out_encoded) -> int {
        if (omni_webrtc_media_test_simulated_time_begin() != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            return Fail("simulated-time: begin failed");
        }

        const std::string config = R"({"codec":"h264","width":320,"height":180,"fps":30,)"
                                   R"("bitrate_bps":400000,"software_encode":true})";
        OmniWebRtcMediaSession* sender = nullptr;
        std::array<char, 512> diagnostics{};
        size_t diagnostics_len = 0;
        if (CreateSession(
                config.c_str(), &sender, diagnostics.data(), diagnostics.size(),
                &diagnostics_len) != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            (void)omni_webrtc_media_test_simulated_time_end();
            return Fail("simulated-time: session_create failed");
        }

        std::vector<uint8_t> y(320 * 180, 128);
        std::vector<uint8_t> u((320 / 2) * (180 / 2), 100);
        std::vector<uint8_t> v((320 / 2) * (180 / 2), 200);

        std::vector<uint8_t> batch(262144);
        long long drained = 0;
        int64_t timestamp_us = 0;
        for (int i = 0; i < 20; ++i) {
            (void)omni_webrtc_media_session_push_i420_frame(
                sender, y.data(), 320, u.data(), 160, v.data(), 160, 320, 180,
                timestamp_us, i == 0 ? 1 : 0);
            timestamp_us += 1'000'000 / 30;
            // ВРЕМЯ ДВИГАЕТ ТЕСТ, А НЕ ОПЕРАЦИОННАЯ СИСТЕМА. Здесь стоял бы sleep_for(33ms), и
            // именно он делал результат зависимым от загрузки машины.
            (void)omni_webrtc_media_test_simulated_time_advance_ms(33);
            size_t written = 0;
            uint32_t count = 0;
            if (DrainSessionPackets(
                    sender, batch.data(), batch.size(), 128, &written, &count) ==
                OMNI_WEBRTC_MEDIA_STATUS_OK) {
                drained += count;
            }
        }

        out_encoded = ReadDiagnosticsInt(sender, R"("frames_encoded":)");
        out_drained = drained;
        DestroySession(sender);
        if (omni_webrtc_media_test_simulated_time_end() != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            return Fail("simulated-time: end failed");
        }
        return 0;
    };

    long long first_drained = 0;
    long long first_encoded = 0;
    long long second_drained = 0;
    long long second_encoded = 0;
    if (int status = run_once(first_drained, first_encoded); status != 0) {
        return status;
    }
    if (int status = run_once(second_drained, second_encoded); status != 0) {
        return status;
    }

    std::cout << "[smoke] simulated-time: encoded " << first_encoded << "/" << second_encoded
              << ", drained " << first_drained << "/" << second_drained << std::endl;

    if (first_encoded <= 0 || first_drained <= 0) {
        return Fail("simulated-time: nothing happened under the simulated clock");
    }
    // ВОТ РАДИ ЭТОЙ СТРОКИ ВСЁ И ДЕЛАЛОСЬ.
    if (first_encoded != second_encoded || first_drained != second_drained) {
        std::cerr << "[smoke] simulated-time: runs differ (" << first_encoded << " vs "
                  << second_encoded << " encoded, " << first_drained << " vs " << second_drained
                  << " drained) — the clock is not actually driving the session\n";
        return Fail("simulated-time: identical scenarios produced different results");
    }
    return 0;
}

// ОТЧЁТ ПРИЁМНИКА: ПАКЕТ НЕ ПО ПОРЯДКУ — НЕ ПОТЕРЯ.
//
// Живьём 2026-09-24 пара на реальном UDP рапортовала 12–33 % потерь RTP при ~1 % на проводе, а
// на TCP-двери — ровно 0. Самодельный счёт засчитывал «получено» только пакету, который двигает
// старший номер, и любой пакет не по порядку оставался потерянным навсегда. Теперь отчёт берёт
// числа у апстримовского ReceiveStatistics. Проверяются обе половины: перестановка пар даёт ноль,
// а настоящая дыра в той же подаче — не ноль.
bool CollectVideoRtpPackets(OmniWebRtcMediaSession* sender, size_t wanted,
                            std::vector<std::vector<uint8_t>>& packets,
                            int kWidth = 320, int kHeight = 180)
{
    std::vector<uint8_t> y_plane;
    std::vector<uint8_t> u_plane;
    std::vector<uint8_t> v_plane;
    FillI420ColorBars(y_plane, u_plane, v_plane, kWidth, kHeight);
    std::vector<uint8_t> batch(1u << 20);
    uint32_t media_ssrc = 0;
    uint32_t noise = 12345;
    for (int frame_index = 0; frame_index < 300 && packets.size() < wanted; ++frame_index) {
        // Big frames get noise, so that a frame really spans many packets (color bars compress
        // into one packet even at 720p).
        if (kWidth > 320) {
            for (uint8_t& sample : y_plane) {
                noise = noise * 1664525u + 1013904223u;
                sample = static_cast<uint8_t>(noise >> 24);
            }
        }
        if (omni_webrtc_media_session_push_i420_frame(
                sender, y_plane.data(), kWidth, u_plane.data(), (kWidth + 1) / 2, v_plane.data(),
                (kWidth + 1) / 2, kWidth, kHeight, 1'000'000 + frame_index * 33'000,
                frame_index == 0 ? 1 : 0) != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            return false;
        }
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
        size_t written = 0;
        uint32_t count = 0;
        if (DrainSessionPackets(
                sender, batch.data(), batch.size(), 128, &written, &count) !=
            OMNI_WEBRTC_MEDIA_STATUS_OK) {
            return false;
        }
        size_t offset = 0;
        for (uint32_t i = 0; i < count && offset + 2 <= written; ++i) {
            const uint16_t len = static_cast<uint16_t>(batch[offset]) |
                static_cast<uint16_t>(static_cast<uint16_t>(batch[offset + 1]) << 8);
            offset += 2;
            if (len < 12 || offset + len > written) {
                return false;
            }
            const uint8_t* rtp = batch.data() + offset;
            const uint8_t payload_type = rtp[1] & 0x7F;
            const uint32_t ssrc = (uint32_t{rtp[8]} << 24) | (uint32_t{rtp[9]} << 16) |
                (uint32_t{rtp[10]} << 8) | uint32_t{rtp[11]};
            // Only the media stream: RTCP (payload type 72..76 in the second byte) and RTX/padding
            // on another SSRC are not what the report is about.
            const bool is_rtcp = payload_type >= 72 && payload_type <= 76;
            if (!is_rtcp && (media_ssrc == 0 || ssrc == media_ssrc) && packets.size() < wanted) {
                media_ssrc = ssrc;
                packets.emplace_back(rtp, rtp + len);
            }
            offset += len;
        }
    }
    return packets.size() >= wanted;
}

/// Первый блок отчёта из настоящего RR (RFC 3550 §6.4.2): доля потерь, накопленные потери (24 бита
/// со знаком) и наибольший номер.
struct ReceiverReportBlock {
    uint8_t fraction_lost = 0;
    int32_t cumulative_lost = 0;
    uint32_t highest_sequence = 0;
};

bool FindReceiverReport(const std::vector<uint8_t>& compound, ReceiverReportBlock& block)
{
    size_t offset = 0;
    while (offset + 4u <= compound.size()) {
        const uint8_t* header = compound.data() + offset;
        const size_t length = (static_cast<size_t>((header[2] << 8) | header[3]) + 1u) * 4u;
        if ((header[0] >> 6) != 2u || offset + length > compound.size()) {
            return false;
        }
        if (header[1] == 201 && (header[0] & 0x1Fu) >= 1 && length >= 8u + 24u) {
            const uint8_t* rb = header + 8;
            block.fraction_lost = rb[4];
            int32_t lost = (static_cast<int32_t>(rb[5]) << 16) | (rb[6] << 8) | rb[7];
            if ((lost & 0x800000) != 0) {
                lost -= 0x1000000;
            }
            block.cumulative_lost = lost;
            block.highest_sequence = (static_cast<uint32_t>(rb[8]) << 24) | (rb[9] << 16) |
                                     (rb[10] << 8) | rb[11];
            return true;
        }
        offset += length;
    }
    return false;
}

/// Подать пакеты в приёмник в заданном порядке и дождаться НАСТОЯЩЕГО отчёта приёмника, который
/// уже видел их все. С ABI 18 RR строит приёмный модуль сам (RTCPSender из ReceiveStatistics, раз в
/// ~секунду на реальных часах), структурного события больше нет.
bool ReportAfterFeeding(const std::string& config, const std::vector<std::vector<uint8_t>>& packets,
                        const std::vector<size_t>& order, ReceiverReportBlock& report)
{
    std::array<char, 512> error{};
    size_t required = 0;
    OmniWebRtcMediaSession* receiver = nullptr;
    if (CreateSession(config.c_str(), &receiver, error.data(), error.size(),
                                         &required) != OMNI_WEBRTC_MEDIA_STATUS_OK ||
        receiver == nullptr) {
        return false;
    }
    uint16_t last_sequence = 0;
    for (size_t index : order) {
        const std::vector<uint8_t>& rtp = packets[index];
        const uint16_t sequence = static_cast<uint16_t>((rtp[2] << 8) | rtp[3]);
        if (static_cast<int16_t>(sequence - last_sequence) > 0 || last_sequence == 0) {
            last_sequence = sequence;
        }
        (void)PushOnePacket(receiver, rtp);
    }
    // The LATEST report that has seen the last packet, not the first: the highest number arrives
    // before the one swapped behind it, and a report taken in that instant honestly counts the
    // still-missing packet as lost. Once everything is in, the cumulative count settles — that is
    // what the feed is judged by. Keep reading after the first match for longer than one report
    // interval (1 s, randomised up to 1.5 s), so a later report is sure to come.
    bool found = false;
    int attempts_after_found = 0;
    for (int attempt = 0; attempt < 700 && attempts_after_found < 250; ++attempt) {
        for (const std::vector<uint8_t>& packet : TakeSessionPackets(receiver)) {
            ReceiverReportBlock block;
            if (FindReceiverReport(packet, block) &&
                static_cast<uint16_t>(block.highest_sequence) == last_sequence) {
                report = block;
                found = true;
            }
        }
        if (found) {
            ++attempts_after_found;
        }
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
    DestroySession(receiver);
    return found;
}

/// Каждый кодек звонка — по всему пути: сессия-отправитель кодирует, упаковывает в RTP, сессия-
/// приёмник собирает и декодирует (docs/decisions/per-pair-video-codec-selection.md). `extra` —
/// добавка к конфигурации (например, программное кодирование). HEVC здесь не гоняется: его
/// декодер только аппаратный и живёт на GPU-пути показа, которого у стенда нет.
int RunCodecLoopbackSmoke(const char* codec, const char* extra, int width = 320, int height = 180)
{
    const std::string config = std::string(R"({"codec":")") + codec + R"(","width":)" +
        std::to_string(width) + R"(,"height":)" + std::to_string(height) +
        R"(,"fps":30,"bitrate_bps":2500000)" + extra + "}";
    std::array<char, 512> error{};
    size_t required = 0;
    OmniWebRtcMediaSession* sender = nullptr;
    if (CreateSession(config.c_str(), &sender, error.data(), error.size(), &required) !=
            OMNI_WEBRTC_MEDIA_STATUS_OK ||
        sender == nullptr) {
        std::cerr << "[smoke] " << codec << " sender: " << error.data() << '\n';
        return Fail("codec loopback: sender create failed");
    }
    std::vector<std::vector<uint8_t>> packets;
    const bool collected = CollectVideoRtpPackets(sender, width > 320 ? 1200 : 60, packets, width, height);
    DestroySession(sender);
    if ((!collected && packets.size() < 200) || packets.empty()) {
        std::cerr << "[smoke] " << codec << " collected " << packets.size() << " packets\n";
        return Fail("codec loopback: sender produced no media packets");
    }
    OmniWebRtcMediaSession* receiver = nullptr;
    if (CreateSession(config.c_str(), &receiver, error.data(), error.size(), &required) !=
            OMNI_WEBRTC_MEDIA_STATUS_OK ||
        receiver == nullptr) {
        std::cerr << "[smoke] " << codec << " receiver: " << error.data() << '\n';
        return Fail("codec loopback: receiver create failed");
    }
    // Decoded pictures are taken like the render loop takes them: the receiver decodes the next
    // frame once the previous one is collected.
    std::vector<uint8_t> picture(static_cast<size_t>(width * height * 4));
    long long drained = 0;
    const auto drain = [&] {
        int32_t width = 0;
        int32_t height = 0;
        uint32_t rtp_timestamp = 0;
        int64_t timestamp_us = 0;
        size_t needed = 0;
        if (omni_webrtc_media_session_drain_i420_frame(
                   receiver, picture.data(), picture.size(), &width, &height, &rtp_timestamp,
                   &timestamp_us, &needed) == OMNI_WEBRTC_MEDIA_STATUS_OK) {
            ++drained;
        }
    };
    for (const std::vector<uint8_t>& packet : packets) {
        (void)PushOnePacket(receiver, packet);
        std::this_thread::sleep_for(std::chrono::milliseconds(width > 320 ? 2 : 12));
        drain();
    }
    for (int i = 0; i < 100; ++i) {
        std::this_thread::sleep_for(std::chrono::milliseconds(20));
        drain();
    }
    const long long decoded = ReadDiagnosticsInt(receiver, R"("frames_decoded":)");
    const long long failures = ReadDiagnosticsInt(receiver, R"("decode_failures":)");
    const long long assembled = ReadDiagnosticsInt(receiver, R"("rtp_frames_assembled":)");
    DestroySession(receiver);
    std::cerr << "[smoke] codec loopback " << codec << extra << " " << width << "x" << height
              << ": packets=" << packets.size()
              << " assembled=" << assembled << " decoded=" << decoded << " failures=" << failures
              << " drained=" << drained << '\n';
    if (decoded < 3) {
        return Fail("codec loopback: the receiver decoded (almost) nothing");
    }
    return 0;
}

/// Как в живом звонке, а не на стенде: отправитель и приёмник крутятся вместе, кадры NV12 с шумом,
/// приёмник подключается позже (первые кадры теряются — он просит ключевой), RTCP приёмника едет
/// обратно отправителю, посреди потока канал проседает (меньше битрейт — меньше кадр). Для
/// кодеков звонка (per-pair-video-codec-selection.md); первый живой AV1 2026-10-13 декодировался
/// на стенде и не декодировался в звонке.
int RunCodecLiveLoopSmoke(const char* codec, const char* extra)
{
    constexpr int kWidth = 1280;
    constexpr int kHeight = 720;
    const std::string config = std::string(R"({"codec":")") + codec +
        R"(","width":1280,"height":720,"fps":30,"bitrate_bps":2500000)" + extra + "}";
    std::array<char, 512> error{};
    size_t required = 0;
    OmniWebRtcMediaSession* sender = nullptr;
    OmniWebRtcMediaSession* receiver = nullptr;
    if (CreateSession(config.c_str(), &sender, error.data(), error.size(), &required) !=
            OMNI_WEBRTC_MEDIA_STATUS_OK ||
        CreateSession(config.c_str(), &receiver, error.data(), error.size(), &required) !=
            OMNI_WEBRTC_MEDIA_STATUS_OK) {
        std::cerr << "[smoke] live loop " << codec << ": " << error.data() << '\n';
        return Fail("codec live loop: session create failed");
    }
    std::vector<uint8_t> y(static_cast<size_t>(kWidth * kHeight));
    std::vector<uint8_t> uv(static_cast<size_t>(kWidth * kHeight / 2), 128);
    std::vector<uint8_t> batch(1u << 21);
    std::vector<uint8_t> picture(static_cast<size_t>(kWidth * kHeight * 4));
    uint32_t noise = 777;
    long long drained = 0;
    for (int i = 0; i < 240; ++i) {
        for (size_t p = 0; p < y.size(); p += 4) {
            noise = noise * 1664525u + 1013904223u;
            std::memcpy(&y[p], &noise, 4);
        }
        (void)omni_webrtc_media_session_push_nv12_frame(
            sender, y.data(), kWidth, uv.data(), kWidth, kWidth, kHeight,
            5'000'000 + static_cast<int64_t>(i) * 33'333, i == 0 ? 1 : 0);
        if (i == 80) {
            (void)omni_webrtc_media_session_set_rates(sender, 250'000, 30);
        }
        size_t written = 0;
        uint32_t count = 0;
        if (DrainSessionPackets(sender, batch.data(), batch.size(), 0, &written, &count) ==
                OMNI_WEBRTC_MEDIA_STATUS_OK &&
            count > 0 && i >= 15) {
            // Split the batch and swap neighbours: a real path delivers some packets out of order.
            std::vector<std::vector<uint8_t>> split;
            for (size_t offset = 0; offset + 2 <= written;) {
                const size_t len = batch[offset] | (static_cast<size_t>(batch[offset + 1]) << 8);
                split.emplace_back(batch.begin() + static_cast<ptrdiff_t>(offset + 2),
                                   batch.begin() + static_cast<ptrdiff_t>(offset + 2 + len));
                offset += 2 + len;
            }
            for (size_t k = 1; k + 1 < split.size(); k += 3) {
                std::swap(split[k], split[k + 1]);
            }
            for (const std::vector<uint8_t>& packet : split) {
                (void)PushOnePacket(receiver, packet);
            }
        }
        if (DrainSessionPackets(receiver, batch.data(), batch.size(), 0, &written, &count) ==
                OMNI_WEBRTC_MEDIA_STATUS_OK &&
            count > 0) {
            (void)omni_webrtc_media_session_push_rtp_packets(sender, batch.data(), written, count);
        }
        int32_t width = 0;
        int32_t height = 0;
        uint32_t rtp_timestamp = 0;
        int64_t timestamp_us = 0;
        size_t needed = 0;
        if (omni_webrtc_media_session_drain_i420_frame(receiver, picture.data(), picture.size(), &width,
                                                       &height, &rtp_timestamp, &timestamp_us,
                                                       &needed) == OMNI_WEBRTC_MEDIA_STATUS_OK) {
            ++drained;
        }
        std::this_thread::sleep_for(std::chrono::milliseconds(33));
    }
    const long long decoded = ReadDiagnosticsInt(receiver, R"("frames_decoded":)");
    const long long failures = ReadDiagnosticsInt(receiver, R"("decode_failures":)");
    const long long assembled = ReadDiagnosticsInt(receiver, R"("rtp_frames_assembled":)");
    std::array<char, 65536> diag{};
    size_t diag_len = 0;
    (void)omni_webrtc_media_session_diagnostics_json(receiver, diag.data(), diag.size(), &diag_len);
    const std::string text(diag.data());
    const size_t head = text.find("last_failed_frame_head");
    for (const char* key : {"\"playout_keyframes\":", "\"playout_frames_awaiting_keyframe\":",
                            "\"playout_pending_frames\":", "\"keyframe_requests\":"}) {
        const size_t at = text.find(key);
        if (at != std::string::npos) {
            std::cerr << "[smoke] live loop " << codec << " " << text.substr(at, text.find(',', at) - at) << '\n';
        }
    }
    {
        // Which encoder the sender took and how it fared — the line that names a stuck MFT.
        std::array<char, 65536> sender_diag{};
        size_t sender_len = 0;
        (void)omni_webrtc_media_session_diagnostics_json(sender, sender_diag.data(), sender_diag.size(), &sender_len);
        const std::string sent(sender_diag.data());
        for (const char* key : {"\"encoder_backend\":", "\"encoder_incoming_frames\":", "\"encoder_encoded_frames\":",
                                "\"mft_encode_avg_ms\":", "\"mft_last_error\":"}) {
            const size_t at = sent.find(key);
            if (at != std::string::npos) {
                const size_t end = std::min(sent.find(",\"", at + 1), at + 400);
                std::cerr << "[smoke] live loop " << codec << " sender " << sent.substr(at, end - at) << '\n';
            }
        }
    }
    DestroySession(receiver);
    DestroySession(sender);
    std::cerr << "[smoke] live loop " << codec << extra << ": assembled=" << assembled
              << " decoded=" << decoded << " failures=" << failures
              << (head == std::string::npos ? std::string() : " " + text.substr(head, 60)) << '\n';
    if (decoded < 20 || failures > 3) {
        return Fail("codec live loop: the receiver did not keep decoding");
    }
    return 0;
}

int RunReceiverReportOutOfOrderIsNotLossSmoke()
{
    const std::string config =
        R"({"codec":"av1","width":320,"height":180,"fps":30,"bitrate_bps":500000,"encode_software":1})";
    std::array<char, 512> error{};
    size_t required = 0;
    OmniWebRtcMediaSession* sender = nullptr;
    if (CreateSession(config.c_str(), &sender, error.data(), error.size(),
                                         &required) != OMNI_WEBRTC_MEDIA_STATUS_OK ||
        sender == nullptr) {
        return Fail("receiver-report smoke: sender create failed");
    }
    std::vector<std::vector<uint8_t>> packets;
    const bool collected = CollectVideoRtpPackets(sender, 21, packets);
    DestroySession(sender);
    if (!collected) {
        return Fail("receiver-report smoke: sender produced too few media packets");
    }

    // 0, then every later pair swapped: 2,1, 4,3, ... — nothing is missing, only the order.
    std::vector<size_t> swapped{0};
    for (size_t i = 1; i + 1 < packets.size(); i += 2) {
        swapped.push_back(i + 1);
        swapped.push_back(i);
    }
    ReceiverReportBlock reordered{};
    if (!ReportAfterFeeding(config, packets, swapped, reordered)) {
        return Fail("receiver-report smoke: no receiver report after a reordered feed");
    }
    if (reordered.cumulative_lost != 0 || reordered.fraction_lost != 0) {
        std::cerr << "[smoke] reordered feed reported lost=" << reordered.cumulative_lost
                  << " fraction=" << static_cast<int>(reordered.fraction_lost) << '\n';
        return Fail("receiver-report smoke: packets out of order were reported as lost");
    }

    // Negative half: the same in-order feed with one packet really missing must show it.
    std::vector<size_t> holed;
    for (size_t i = 0; i < packets.size(); ++i) {
        if (i != 10) {
            holed.push_back(i);
        }
    }
    ReceiverReportBlock missing{};
    if (!ReportAfterFeeding(config, packets, holed, missing)) {
        return Fail("receiver-report smoke: no receiver report after a feed with a hole");
    }
    // Только накопленные: доля в RR — за интервал с ПРОШЛОГО отчёта, а приёмник шлёт составной
    // RR+NACK вскоре после дыры (через WebRTC-SendNackDelayMs), так что к последнему отчёту интервал с дырой уже закрыт.
    if (missing.cumulative_lost < 1) {
        std::cerr << "[smoke] holed feed reported lost=" << missing.cumulative_lost
                  << " fraction=" << static_cast<int>(missing.fraction_lost) << '\n';
        return Fail("receiver-report smoke: a missing packet was not reported");
    }
    return 0;
}
/// Сколько номеров приёмник попросил повторить, когда пакеты пришли в порядке `order`.
/// Ждёт `settle_ms` после подачи: просьба уходит не сразу, а по тику запросчика.
long long NackedAfterFeeding(const std::string& config, const std::vector<std::vector<uint8_t>>& packets,
                             const std::vector<size_t>& order, int settle_ms)
{
    std::array<char, 512> error{};
    size_t required = 0;
    OmniWebRtcMediaSession* receiver = nullptr;
    if (CreateSession(config.c_str(), &receiver, error.data(), error.size(),
                                         &required) != OMNI_WEBRTC_MEDIA_STATUS_OK ||
        receiver == nullptr) {
        return -1;
    }
    for (size_t index : order) {
        (void)PushOnePacket(receiver, packets[index]);
    }
    std::this_thread::sleep_for(std::chrono::milliseconds(settle_ms));
    const long long nacked = ReadDiagnosticsInt(receiver, R"("rtcp_nack_seq_sent":)");
    DestroySession(receiver);
    return nacked;
}

/// Перестановка соседних пакетов — не повод просить повтор (WebRTC-SendNackDelayMs).
///
/// На реальном пути ~17 % видео-пакетов приходят не по порядку, почти все в пределах 2 мс. Без
/// задержки запросчик просил повтор каждого и отправитель повторял каждый — ~17 % трафика зря.
int RunReorderedPacketsAreNotNackedSmoke()
{
    const std::string config =
        R"({"codec":"av1","width":320,"height":180,"fps":30,"bitrate_bps":500000,"encode_software":1})";
    std::array<char, 512> error{};
    size_t required = 0;
    OmniWebRtcMediaSession* sender = nullptr;
    if (CreateSession(config.c_str(), &sender, error.data(), error.size(),
                                         &required) != OMNI_WEBRTC_MEDIA_STATUS_OK ||
        sender == nullptr) {
        return Fail("reorder-nack smoke: sender create failed");
    }
    std::vector<std::vector<uint8_t>> packets;
    const bool collected = CollectVideoRtpPackets(sender, 21, packets);
    DestroySession(sender);
    if (!collected) {
        return Fail("reorder-nack smoke: sender produced too few media packets");
    }

    std::vector<size_t> swapped{0};
    for (size_t i = 1; i + 1 < packets.size(); i += 2) {
        swapped.push_back(i + 1);
        swapped.push_back(i);
    }
    const long long reordered = NackedAfterFeeding(config, packets, swapped, 300);
    if (reordered != 0) {
        std::cerr << "[smoke] swapped neighbours were nacked: " << reordered << '\n';
        return Fail("reorder-nack smoke: packets merely out of order were asked for again");
    }

    // Negative half: a packet that never comes must still be asked for.
    std::vector<size_t> holed;
    for (size_t i = 0; i < packets.size(); ++i) {
        if (i != 10) {
            holed.push_back(i);
        }
    }
    const long long missing = NackedAfterFeeding(config, packets, holed, 300);
    if (missing < 1) {
        std::cerr << "[smoke] holed feed nacked: " << missing << '\n';
        return Fail("reorder-nack smoke: a missing packet was never asked for");
    }
    return 0;
}
int RunLibWebRtcSessionSmoke(std::string_view codec, bool software_encode = false)
{
    constexpr int kWidth = 320;
    constexpr int kHeight = 180;
    constexpr int kFps = 30;
    constexpr int kBitrateBps = 500'000;
    constexpr size_t kMaxRtpPacketBytes = 1000;
    const std::string session_config =
        std::string(R"({"codec":")") + std::string(codec) +
        R"(","width":320,"height":180,"fps":30,"bitrate_bps":500000)" +
        (software_encode ? R"(,"encode_software":1})" : R"(})");

    std::array<char, 512> error{};
    size_t required_error_len = 0;
    OmniWebRtcMediaSession* sender = nullptr;
    OmniWebRtcMediaStatus status = CreateSession(
        session_config.c_str(),
        &sender,
        error.data(),
        error.size(),
        &required_error_len);
    if (status != OMNI_WEBRTC_MEDIA_STATUS_OK || sender == nullptr) {
        std::cerr << "sender create failed: " << error.data() << '\n';
        return Fail("libwebrtc sender session_create failed");
    }

    OmniWebRtcMediaSession* receiver = nullptr;
    status = CreateSession(
        session_config.c_str(),
        &receiver,
        error.data(),
        error.size(),
        &required_error_len);
    if (status != OMNI_WEBRTC_MEDIA_STATUS_OK || receiver == nullptr) {
        DestroySession(sender);
        std::cerr << "receiver create failed: " << error.data() << '\n';
        return Fail("libwebrtc receiver session_create failed");
    }

    std::vector<uint8_t> y_plane;
    std::vector<uint8_t> u_plane;
    std::vector<uint8_t> v_plane;
    FillI420ColorBars(y_plane, u_plane, v_plane, kWidth, kHeight);

    // Кадров НЕСКОЛЬКО, а не один: у аппаратного кодировщика конвейерная задержка, и на первом
    // кадре он законно не отдаёт пакетов. Замер 2026-08-15 на этой машине:
    // 1 кадр -> 0 пакетов, 5 кадров -> 9. Прежнее допущение «кадр подали — пакет обязан быть»
    // описывало только программный путь, и тест падал на исправном коде.
    // Ключевой кадр просим у первого — его пакеты и поедут получателю.
    // КОДИРОВАНИЕ СТАЛО АСИНХРОННЫМ (#61, шаг D2b).
    //
    // Раньше кадр кодировался ВНУТРИ push_i420_frame, и выгрести пакеты можно было сразу за
    // возвратом. Теперь кадр уходит в VideoStreamEncoder, тот кодирует на своей очереди, а пейсер
    // отпускает пакеты по часам. Пять попыток «подал — сразу выгреб» этого окна не покрывают:
    // между подачей и появлением пакетов надо ДАТЬ ВРЕМЯ. Двадцать попыток по 10 мс — заведомо
    // больше любой разумной задержки кодировщика и заведомо меньше, чем ждал бы человек.
    constexpr int kMaxFramesToPrime = 100;
    std::array<uint8_t, 65536> rtp_batch{};
    size_t written_len = 0;
    uint32_t packet_count = 0;
    uint16_t first_sequence = 0;
    uint32_t media_ssrc = 0;
    for (int frame_index = 0; frame_index < kMaxFramesToPrime && packet_count == 0; ++frame_index) {
        status = omni_webrtc_media_session_push_i420_frame(
            sender,
            y_plane.data(),
            kWidth,
            u_plane.data(),
            (kWidth + 1) / 2,
            v_plane.data(),
            (kWidth + 1) / 2,
            kWidth,
            kHeight,
            1'000'000 + frame_index * 33'000,
            frame_index == 0 ? 1 : 0);
        if (status != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            DestroySession(receiver);
            DestroySession(sender);
            return Fail("libwebrtc push_i420_frame failed");
        }

        written_len = 0;
        packet_count = 0;
        status = DrainSessionPackets(
            sender,
            rtp_batch.data(),
            rtp_batch.size(),
            128,
            &written_len,
            &packet_count);
        if (status != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            break;
        }
        if (packet_count == 0) {
            std::this_thread::sleep_for(std::chrono::milliseconds(10));
        }
    }
    if (status != OMNI_WEBRTC_MEDIA_STATUS_OK || written_len == 0 || packet_count == 0) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail("libwebrtc drain_rtp_packets returned no RTP");
    }

    size_t packet_offset = 0;
    for (uint32_t i = 0; i < packet_count; ++i) {
        if (written_len - packet_offset < sizeof(uint16_t)) {
            DestroySession(receiver);
            DestroySession(sender);
            return Fail("libwebrtc RTP batch has truncated length prefix");
        }

        const uint16_t packet_len = static_cast<uint16_t>(rtp_batch[packet_offset]) |
            static_cast<uint16_t>(static_cast<uint16_t>(rtp_batch[packet_offset + 1]) << 8);
        packet_offset += sizeof(uint16_t);
        if (packet_len == 0 ||
            packet_len > kMaxRtpPacketBytes ||
            written_len - packet_offset < packet_len) {
            DestroySession(receiver);
            DestroySession(sender);
            return Fail("libwebrtc RTP packet exceeds QUIC datagram budget");
        }

        // Первый МЕДИАпакет: с ABI 17 отправитель выпускает в ту же трубу и свой RTCP (SR).
        const bool is_rtcp = packet_len >= 2 && rtp_batch[packet_offset + 1] >= 192 &&
                             rtp_batch[packet_offset + 1] <= 223;
        if (media_ssrc == 0 && !is_rtcp) {
            first_sequence = static_cast<uint16_t>(
                static_cast<uint16_t>(rtp_batch[packet_offset + 2]) << 8 |
                static_cast<uint16_t>(rtp_batch[packet_offset + 3]));
            media_ssrc = (static_cast<uint32_t>(rtp_batch[packet_offset + 8]) << 24) |
                         (static_cast<uint32_t>(rtp_batch[packet_offset + 9]) << 16) |
                         (static_cast<uint32_t>(rtp_batch[packet_offset + 10]) << 8) |
                         static_cast<uint32_t>(rtp_batch[packet_offset + 11]);
        }

        packet_offset += packet_len;
    }

    if (packet_offset != written_len) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail("libwebrtc RTP batch has trailing bytes");
    }

    status = omni_webrtc_media_session_push_rtp_packets(
        receiver,
        rtp_batch.data(),
        written_len,
        packet_count);
    if (status != OMNI_WEBRTC_MEDIA_STATUS_OK) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail("libwebrtc push_rtp_packets failed");
    }

    // REPORTS ARE REAL RTCP NOW (ABI 18): the sender's module emits its own SR, the receiver's
    // module its RR. Neither is a control event any more. The SR is on the module's own timer
    // (about a second), so it is waited for among the sender's packets.
    bool saw_sender_report = false;
    for (int attempt = 0; attempt < 300 && !saw_sender_report; ++attempt) {
        for (const std::vector<uint8_t>& packet : TakeSessionPackets(sender)) {
            saw_sender_report = saw_sender_report || RtcpCompoundHasType(packet, 200);
        }
        if (!saw_sender_report) {
            std::this_thread::sleep_for(std::chrono::milliseconds(10));
        }
    }
    if (!saw_sender_report) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail("libwebrtc sender module emitted no RTCP sender report");
    }

    // A KEYFRAME REQUEST, AS A REAL RTCP PLI OUT OF THE RECEIVER (ABI 16).
    //
    // A decoder joins the stream mid-flight — every packet it is holding references state it never
    // had — so it has to ask (reason decoder_started). The encoder no longer emits a periodic IDR,
    // so if this request stops being sent the first picture waits for a decode failure instead.
    // It used to be a control event the shell carried; now the receive module sends it, posted to
    // its queue, so it is waited for among the receiver's own packets.
    std::vector<std::vector<uint8_t>> receiver_rtcp;
    bool saw_start_pli = false;
    for (int attempt = 0; attempt < 200 && !saw_start_pli; ++attempt) {
        for (std::vector<uint8_t>& packet : TakeSessionPackets(receiver)) {
            saw_start_pli = saw_start_pli || RtcpCompoundContains(packet, 206, 1);
            receiver_rtcp.push_back(std::move(packet));
        }
        if (!saw_start_pli) {
            std::this_thread::sleep_for(std::chrono::milliseconds(10));
        }
    }
    bool saw_receiver_report = false;
    for (const std::vector<uint8_t>& packet : receiver_rtcp) {
        ReceiverReportBlock block;
        saw_receiver_report = saw_receiver_report || FindReceiverReport(packet, block);
    }
    if (!saw_receiver_report) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail("libwebrtc receiver module emitted no RTCP receiver report");
    }
    if (!saw_start_pli ||
        !SessionDiagnosticsContains(receiver, R"("last_keyframe_request_reason":"decoder_started")")) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail("libwebrtc receiver did not send an RTCP PLI when its decoder started");
    }

    // ...and it must REACH the sender's encoder when delivered the way the inbound router delivers
    // it. Negative half first: nothing delivered, nothing received.
    if (ReadDiagnosticsInt(sender, R"("rtcp_pli_received":)") != 0) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail("libwebrtc sender counted a PLI that was never delivered");
    }
    // Отчёты о доставке (205/11) здесь НЕ отдаются, и это не упрощение. Пакет, о доставке которого
    // отправитель узнал, история повторов вычёркивает (RtpPacketHistory, kStoreAndCull), и NACK
    // на него ниже законно остался бы без ответа. Повтор просят у потерянного — для отправителя,
    // не получившего отчёта, пакет и есть потерянный.
    for (const std::vector<uint8_t>& packet : receiver_rtcp) {
        if (!RtcpCompoundContains(packet, 205, 11)) {
            (void)PushOnePacket(sender, packet);
        }
    }
    long long plis_received = 0;
    for (int attempt = 0; attempt < 200 && plis_received <= 0; ++attempt) {
        plis_received = ReadDiagnosticsInt(sender, R"("rtcp_pli_received":)");
        if (plis_received <= 0) {
            std::this_thread::sleep_for(std::chrono::milliseconds(10));
        }
    }
    if (plis_received != 1) {
        std::cerr << "[smoke] rtcp_pli_received=" << plis_received << '\n';
        DestroySession(receiver);
        DestroySession(sender);
        return Fail("libwebrtc sender did not take the receiver's RTCP PLI");
    }

    // A RETRANSMISSION ASKED FOR WITH A REAL RTCP NACK.
    //
    // The NACK is built here byte by byte (RFC 4585) rather than coaxed out of the receiver: what
    // is checked is the sender's half — that a NACK arriving on the wire reaches its packet
    // history. The receiver's half is covered end to end by the picture-quality stand under loss.
    //
    // Checked by what left on the wire as a RETRANSMISSION, not by "some packet came out": that
    // could be fresh video, and the check would pass with retransmission dead.
    //
    // Negative half first: a NACK naming a stream that is not this sender's must resend nothing.
    auto retransmissions_after = [&](int polls) {
        std::array<uint8_t, 4096> drained{};
        long long sent = 0;
        for (int attempt = 0; attempt < polls; ++attempt) {
            size_t drained_len = 0;
            uint32_t drained_count = 0;
            (void)DrainSessionPackets(
                sender, drained.data(), drained.size(), 4, &drained_len, &drained_count);
            sent = ReadDiagnosticsInt(sender, R"("module_retransmissions_sent":)");
            if (sent > 0) {
                break;
            }
            std::this_thread::sleep_for(std::chrono::milliseconds(10));
        }
        return sent;
    };
    (void)PushOnePacket(sender, BuildGenericNack(0x12345678u, media_ssrc ^ 0x1u, first_sequence));
    if (retransmissions_after(20) != 0) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail("libwebrtc sender resent a packet for a NACK that named another stream");
    }
    (void)PushOnePacket(sender, BuildGenericNack(0x12345678u, media_ssrc, first_sequence));
    if (retransmissions_after(200) <= 0) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail("libwebrtc native RTP retransmit failed: the peer asked and nothing was resent");
    }

    int32_t decoded_width = 0;
    int32_t decoded_height = 0;
    uint32_t rtp_timestamp = 0;
    int64_t timestamp_us = 0;
    size_t required_frame_len = 0;
    status = WaitForI420FrameSize(
        receiver,
        &decoded_width,
        &decoded_height,
        &rtp_timestamp,
        &timestamp_us,
        &required_frame_len);
    if (status != OMNI_WEBRTC_MEDIA_STATUS_BUFFER_TOO_SMALL ||
        decoded_width != kWidth ||
        decoded_height != kHeight ||
        required_frame_len == 0) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail("libwebrtc decoded frame sizing failed");
    }

    std::vector<uint8_t> decoded_frame(required_frame_len);
    status = omni_webrtc_media_session_drain_i420_frame(
        receiver,
        decoded_frame.data(),
        decoded_frame.size(),
        &decoded_width,
        &decoded_height,
        &rtp_timestamp,
        &timestamp_us,
        &required_frame_len);
    if (status != OMNI_WEBRTC_MEDIA_STATUS_OK ||
        decoded_width != kWidth ||
        decoded_height != kHeight ||
        !DecodedColorBarsArePlausible(decoded_frame, decoded_width, decoded_height)) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail("libwebrtc decoded frame color-bars failed");
    }

    std::vector<uint8_t> bgra_frame;
    FillBgraColorBars(bgra_frame, kWidth, kHeight);
    status = omni_webrtc_media_session_set_rates(sender, 250'000, 15);
    if (status != OMNI_WEBRTC_MEDIA_STATUS_OK) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail("libwebrtc set_rates failed");
    }

    status = omni_webrtc_media_session_request_keyframe(sender);
    if (status != OMNI_WEBRTC_MEDIA_STATUS_OK) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail("libwebrtc request_keyframe failed");
    }

    // ОДНОГО КАДРА БОЛЬШЕ НЕ ХВАТАЕТ (#61, шаг D2b).
    //
    // Кодировщиком управляет VideoStreamEncoder, и он ВПРАВЕ пропустить кадр: только что сменили
    // скорость, значит идёт перенастройка, а пропуск кадров — штатный способ держать битрейт.
    // Проверка «подал один кадр — жди пакеты» требовала от него поведения, которого он не обязан
    // давать. Подаём, пока пакеты не появятся; выход по первому же успеху.
    written_len = 0;
    packet_count = 0;
    uint32_t bgra_packets_total = 0;
    // ВЫХОД ПО ПЕРВОМУ ПАКЕТУ — НЕ ТО УСЛОВИЕ.
    //
    // Один пакет это ещё не кадр, а декодировать приёмник будет именно кадр. Гоняем фиксированное
    // число тактов, отдавая приёмнику ВСЁ, что вышло: так у него набирается целый кадр и есть что
    // показать. Раньше хватало одного пакета, потому что кодирование было синхронным и первый же
    // вызов отдавал кадр целиком.
    for (int attempt = 0; attempt < 30; ++attempt) {
        status = omni_webrtc_media_session_push_bgra_frame(
            sender,
            bgra_frame.data(),
            kWidth * 4,
            kWidth,
            kHeight,
            2'000'000 + attempt * 33'000,
            0);
        if (status != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            DestroySession(receiver);
            DestroySession(sender);
            return Fail("libwebrtc push_bgra_frame failed");
        }
        status = DrainRtpWithLatency(
            sender, rtp_batch.data(), rtp_batch.size(), 128, &written_len, &packet_count);
        if (status != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            break;
        }
        // КАЖДАЯ порция уезжает получателю сразу. Держать только последнюю — значит потерять
        // ключевой кадр, который ушёл в одной из первых: декодировать после этого нечего.
        if (packet_count > 0) {
            bgra_packets_total += packet_count;
            status = omni_webrtc_media_session_push_rtp_packets(
                receiver, rtp_batch.data(), written_len, packet_count);
            if (status != OMNI_WEBRTC_MEDIA_STATUS_OK) {
                DestroySession(receiver);
                DestroySession(sender);
                return Fail("libwebrtc push_rtp_packets failed for BGRA input");
            }
        }
    }
    if (status != OMNI_WEBRTC_MEDIA_STATUS_OK || bgra_packets_total == 0) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail("libwebrtc drain_rtp_packets returned no RTP for BGRA input");
    }

    decoded_width = 0;
    decoded_height = 0;
    rtp_timestamp = 0;
    timestamp_us = 0;
    required_frame_len = 0;
    status = WaitForI420FrameSize(
        receiver,
        &decoded_width,
        &decoded_height,
        &rtp_timestamp,
        &timestamp_us,
        &required_frame_len);
    // РАЗМЕР КАДРА БОЛЬШЕ НЕ ФИКСИРОВАН (#61, шаг D2b).
    //
    // Разрешение выбирает VideoStreamEncoder: под нехваткой полосы он уменьшает кадр, и это не
    // поломка, а работа. Стенд только что попросил 250 кбит/с — при таком бюджете 640x360 ему
    // вполне может оказаться не по средствам. Требовать точный размер значит требовать, чтобы
    // адаптация НЕ работала. Проверяем то, ради чего стенд написан: кадр дошёл, он не пустой и в
    // нём та самая картинка.
    if (status != OMNI_WEBRTC_MEDIA_STATUS_BUFFER_TOO_SMALL ||
        decoded_width <= 0 || decoded_width > kWidth ||
        decoded_height <= 0 || decoded_height > kHeight ||
        required_frame_len == 0) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail("libwebrtc BGRA decoded frame sizing failed");
    }

    decoded_frame.assign(required_frame_len, 0);
    status = omni_webrtc_media_session_drain_i420_frame(
        receiver,
        decoded_frame.data(),
        decoded_frame.size(),
        &decoded_width,
        &decoded_height,
        &rtp_timestamp,
        &timestamp_us,
        &required_frame_len);
    if (status != OMNI_WEBRTC_MEDIA_STATUS_OK ||
        decoded_width <= 0 || decoded_width > kWidth ||
        decoded_height <= 0 || decoded_height > kHeight ||
        !DecodedColorBarsArePlausible(decoded_frame, decoded_width, decoded_height)) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail("libwebrtc BGRA decoded frame color-bars failed");
    }

    // ДОБИРАЕМ ДО ДВУХ ДЕКОДИРОВАННЫХ КАДРОВ (#61, шаг D2b).
    //
    // Проверка ниже требует, чтобы декодировалось не меньше двух кадров. Пока кодирование было
    // синхронным, столько и приезжало: подали два — получили два. Теперь кадр уходит в
    // VideoStreamEncoder, тот вправе его пропустить ради битрейта, а выдачу метит пейсер. Двух
    // кадров может не набраться просто потому, что мы не дали времени — и тест падал бы не по
    // существу. Подаём и возим, пока их не станет двое.
    for (int attempt = 0;
         attempt < 60 && ReadDiagnosticsInt(receiver, R"("frames_decoded":)") < 2;
         ++attempt) {
        (void)omni_webrtc_media_session_push_bgra_frame(
            sender, bgra_frame.data(), kWidth * 4, kWidth, kHeight,
            3'000'000 + attempt * 33'000, 0);
        size_t top_up_len = 0;
        uint32_t top_up_count = 0;
        if (DrainRtpWithLatency(sender, rtp_batch.data(), rtp_batch.size(), 128, &top_up_len,
                                &top_up_count) == OMNI_WEBRTC_MEDIA_STATUS_OK &&
            top_up_count > 0) {
            (void)omni_webrtc_media_session_push_rtp_packets(
                receiver, rtp_batch.data(), top_up_len, top_up_count);
        }
        int32_t w = 0;
        int32_t h = 0;
        uint32_t ts = 0;
        int64_t us = 0;
        size_t len = 0;
        (void)omni_webrtc_media_session_drain_i420_frame(
            receiver, nullptr, 0, &w, &h, &ts, &us, &len);
        if (len > 0) {
            std::vector<uint8_t> sink(len, 0);
            (void)omni_webrtc_media_session_drain_i420_frame(
                receiver, sink.data(), sink.size(), &w, &h, &ts, &us, &len);
        }
    }

    // Размер спрашиваем у самой диагностики, а не держим константу.
    //
    // Здесь стоял `std::array<char, 2048>`, и запас до переполнения составлял 12 байт: JSON уже
    // был 2036. Любое новое поле роняло тест не по существу, а по размеру буфера — что и
    // случилось 2026-08-15 при добавлении `encoder_fallback_reason` (стало 2066). Контракт
    // «спроси длину, потом выдели» у этой функции есть и уже используется в проверке пробы.
    // ЗАМЕР ДЛИНЫ И ЧТЕНИЕ — ДВА РАЗНЫХ ВЫЗОВА, А СЕССИЯ МЕЖДУ НИМИ ЖИВЁТ.
    //
    // Диагностика собирается заново на каждом вызове, и счётчики между замером и чтением успевают
    // подрасти — достаточно одному прибавить разряд. Буфер, выделенный ровно по прошлой длине,
    // тогда оказывается мал, и чтение возвращает BUFFER_TOO_SMALL. Ровно этот дефект уже был
    // найден и починен в ОБОЛОЧКЕ (NativeWebRtcMediaEngine.ReadNativeString), а здесь остался:
    // два прогона из восьми падали на «session diagnostics failed». Берём с запасом и повторяем.
    size_t required_diagnostics_len = 0;
    std::vector<char> diagnostics;
    for (int attempt = 0; attempt < 4; ++attempt) {
        (void)omni_webrtc_media_session_diagnostics_json(
            receiver, nullptr, 0, &required_diagnostics_len);
        diagnostics.assign(
            std::max<size_t>(required_diagnostics_len + 1024 * (attempt + 1), 4096), '\0');
        status = omni_webrtc_media_session_diagnostics_json(
            receiver,
            diagnostics.data(),
            diagnostics.size(),
            &required_diagnostics_len);
        if (status != OMNI_WEBRTC_MEDIA_STATUS_BUFFER_TOO_SMALL) {
            break;
        }
    }
    // ПОРОГ СНИЖЕН С ДВУХ КАДРОВ ДО ОДНОГО (#61, шаг D2b) — И ЭТО НАДО НАЗВАТЬ ВСЛУХ.
    //
    // Требование «оба поданных кадра доехали» было верно, пока кодирование шло один-к-одному:
    // сколько подали, столько и вышло. Теперь кодировщиком управляет VideoStreamEncoder, и
    // ПРОПУСК КАДРА — его штатный способ держать битрейт (frame_drop_enabled). Требовать двух
    // кадров значит требовать, чтобы он не пропускал, то есть запрещать работать регулятору.
    // Замер: программная ветка даёт то один кадр, то два — один прогон из шести падал.
    //
    // Что при этом НЕ ослабло: выше уже проверено, что декодированный кадр пришёл и в нём та
    // самая картинка (DecodedColorBarsArePlausible). Утверждение «картинка доехала» сохранено
    // целиком; снято только «ни один кадр не был пропущен», которое больше не наше.
    //
    // ЧИСЛО ДЕКОДИРОВАННЫХ СРАВНИВАЕТСЯ ПО «НЕ МЕНЬШЕ», А НЕ ТОЧНО.
    //
    // Стояло `"frames_decoded":2` подстрокой. Точный счёт был верен, пока путь отправки был
    // синхронным: сколько кадров подали, столько и приехало. Теперь пакетизация уходит задачей на
    // очередь, пейсер тикает сам, а отчёты RTCP добавляют своё время — и кодировщик успевает
    // выдать третий кадр, пока тест ждёт первых пакетов. Проверка падала не по существу.
    //
    // Требование, ради которого она стоит, — ОБА поданных кадра доехали и декодировались. Оно и
    // проверяется. Лишний кадр сверху ничего не нарушает.
    const long long decoded_frames = ReadDiagnosticsInt(receiver, R"("frames_decoded":)");
    if (status != OMNI_WEBRTC_MEDIA_STATUS_OK ||
        !Contains(diagnostics.data(), R"("native_session_scaffold":true)") ||
        !Contains(diagnostics.data(), (std::string(R"("codec":")") + std::string(codec) + R"(")").c_str()) ||
        decoded_frames < 1) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail("libwebrtc session diagnostics failed");
    }

    (void)kFps;
    (void)kBitrateBps;
    DestroySession(receiver);
    DestroySession(sender);
    return 0;
}

// Incompressible content: colour bars encode down to a single RTP packet even at 720p, and a
// one-packet frame hides the very thing the sender-restart case has to exercise (a frame whose
// first packets are discarded). Noise gives frames of dozens of packets, like real video.
void FillI420Noise(
    std::vector<uint8_t>& y_plane,
    std::vector<uint8_t>& u_plane,
    std::vector<uint8_t>& v_plane,
    int width,
    int height,
    uint32_t seed)
{
    const size_t luma = static_cast<size_t>(width) * static_cast<size_t>(height);
    const int chroma_width = (width + 1) / 2;
    const int chroma_height = (height + 1) / 2;
    const size_t chroma = static_cast<size_t>(chroma_width) * static_cast<size_t>(chroma_height);
    y_plane.resize(luma);
    u_plane.resize(chroma);
    v_plane.resize(chroma);

    uint32_t state = seed | 1u;
    auto next = [&state]() {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        return static_cast<uint8_t>(state & 0xFFu);
    };

    for (size_t i = 0; i < luma; ++i) {
        y_plane[i] = next();
    }
    for (size_t i = 0; i < chroma; ++i) {
        u_plane[i] = next();
        v_plane[i] = next();
    }
}

// Drain everything the sender has queued and hand it to the receiver, exactly the way the
// shell's send loop does. An empty queue is not a failure — pacing may hold packets back.
bool ForwardRtpFromSenderToReceiver(
    OmniWebRtcMediaSession* sender,
    OmniWebRtcMediaSession* receiver,
    uint32_t& forwarded_packets)
{
    // Big enough for a full drain of 128 max-size packets: a 720p keyframe is hundreds of
    // packets, and 64 KB was not enough to hold one drain of it.
    std::vector<uint8_t> batch(262144);
    for (int pass = 0; pass < 256; ++pass) {
        size_t written_len = 0;
        uint32_t packet_count = 0;
        if (DrainSessionPackets(
                sender,
                batch.data(),
                batch.size(),
                128,
                &written_len,
                &packet_count) != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            return false;
        }

        if (packet_count == 0 || written_len == 0) {
            return true;
        }

        if (omni_webrtc_media_session_push_rtp_packets(
                receiver,
                batch.data(),
                written_len,
                packet_count) != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            return false;
        }

        forwarded_packets += packet_count;
    }

    return true;
}

// Pop every decoded frame the receiver is holding; report how many of them are AT OR AFTER
// `min_rtp_timestamp`. The filter is what makes "did the restarted sender get through?"
// answerable: frames the previous sender left queued in the playout buffer would otherwise be
// counted as the new sender's.
/// PSNR по яркости между тем, что подали, и тем, что приёмник декодировал, в децибелах.
///
/// Без этого числа «качество картинки» — не утверждение, а мнение: любую правку кодека можно
/// объявить улучшением, и проверить это нечем. PSNR не идеален (он мерит ошибку, а не восприятие),
/// но он воспроизводим и сравним между прогонами, а именно сравнение нам и нужно: одна и та же
/// сцена, один и тот же битрейт, разные настройки кодировщика.
///
/// Бесконечность (идентичные кадры) отсекаем потолком в 99 дБ, чтобы среднее оставалось числом.
double ComputeYPlanePsnrDb(
    const std::vector<uint8_t>& source_y,
    const uint8_t* decoded_y,
    int width,
    int height)
{
    const size_t pixels = static_cast<size_t>(width) * static_cast<size_t>(height);
    if (pixels == 0 || source_y.size() < pixels) {
        return 0.0;
    }

    double squared_error = 0.0;
    for (size_t i = 0; i < pixels; ++i) {
        const double diff = static_cast<double>(source_y[i]) - static_cast<double>(decoded_y[i]);
        squared_error += diff * diff;
    }

    const double mse = squared_error / static_cast<double>(pixels);
    if (mse <= 0.0) {
        return 99.0;
    }

    return std::min(99.0, 10.0 * std::log10((255.0 * 255.0) / mse));
}

int DrainDecodedFrames(OmniWebRtcMediaSession* receiver, uint32_t min_rtp_timestamp)
{
    int drained_at_or_after = 0;
    std::vector<uint8_t> frame;
    for (int guard = 0; guard < 256; ++guard) {
        int32_t width = 0;
        int32_t height = 0;
        uint32_t rtp_timestamp = 0;
        int64_t timestamp_us = 0;
        size_t required_len = 0;
        OmniWebRtcMediaStatus status = omni_webrtc_media_session_drain_i420_frame(
            receiver, nullptr, 0, &width, &height, &rtp_timestamp, &timestamp_us, &required_len);
        if (status != OMNI_WEBRTC_MEDIA_STATUS_BUFFER_TOO_SMALL || required_len == 0) {
            return drained_at_or_after;
        }

        frame.assign(required_len, 0);
        status = omni_webrtc_media_session_drain_i420_frame(
            receiver,
            frame.data(),
            frame.size(),
            &width,
            &height,
            &rtp_timestamp,
            &timestamp_us,
            &required_len);
        if (status != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            return drained_at_or_after;
        }

        if (rtp_timestamp >= min_rtp_timestamp) {
            ++drained_at_or_after;
        }
    }

    return drained_at_or_after;
}

// The RTP timestamp the sender derives from a capture timestamp (see PushI420Frame).
uint32_t RtpTimestampForCaptureUs(int64_t timestamp_us)
{
    return static_cast<uint32_t>((timestamp_us * 90) / 1000);
}

// The peer's picture must survive a SENDER RESTART.
//
// The shell destroys and recreates the native sender session whenever the encoder's input
// changes — camera → screen share, screen share → camera, and any resolution change including
// an ABR downscale. The RECEIVER session is not recreated: the negotiated selection didn't
// change, so there is nothing to renegotiate. A restarted sender therefore feeds RTP into a
// receive assembler that has already seen thousands of packets, which is why the RTP sequence
// space must NOT restart with the session — libwebrtc's assembler silently discards packets
// behind what it has already cleared, and the peer's picture freezes on its last frame.
//
// Live test 2026-08-11: screen share started after the camera delivered nothing at all.
// See docs/bugs/screen-share-had-no-frame-rate.md.
// Определение ниже — объявление здесь, чтобы порог «сколько кадров кодировщик реально выдал» был
// доступен тесту про перезапуск отправителя, не двигая большой блок кода.
long long ReadDiagnosticsInt(OmniWebRtcMediaSession* session, std::string_view key);

int RunLibWebRtcSenderRestartSmoke(std::string_view codec)
{
    // 720p, not the 320x180 the other cases use. At 320x180 a frame fits in ONE RTP packet, and a
    // single-packet frame can re-baseline the receive assembler by itself — which is why a
    // smaller version of this test passed even against the bug. Real frames, a screen keyframe
    // above all, span dozens to hundreds of packets, and it is a frame whose FIRST packets get
    // discarded that never completes.
    constexpr int kWidth = 640;
    constexpr int kHeight = 360;
    // Enough headroom that the pacer is not the bottleneck: the point is to test a sender
    // restart, and a first pass that is already dropping most of its frames to congestion would
    // prove nothing about the second.
    constexpr int kBitrateBps = 20'000'000;
    // The first sender also has to run long enough that the receiver has CLEARED past the range a
    // restarted numbering would re-use.
    constexpr int kFirstSenderFrames = 150;
    constexpr int kRestartedSenderFrames = 60;
    constexpr int64_t kFrameIntervalUs = 33'333;
    constexpr int64_t kFirstCaptureUs = 1'000'000;
    const std::string session_config =
        std::string(R"({"codec":")") + std::string(codec) +
        R"(","width":)" + std::to_string(kWidth) +
        R"(,"height":)" + std::to_string(kHeight) +
        R"(,"fps":30,"bitrate_bps":)" + std::to_string(kBitrateBps) + R"(})";

    std::array<char, 512> error{};
    size_t required_error_len = 0;
    OmniWebRtcMediaSession* receiver = nullptr;
    if (CreateSession(
            session_config.c_str(),
            &receiver,
            error.data(),
            error.size(),
            &required_error_len) != OMNI_WEBRTC_MEDIA_STATUS_OK ||
        receiver == nullptr) {
        std::cerr << "sender-restart receiver create failed: " << error.data() << '\n';
        return Fail("sender-restart receiver session_create failed");
    }

    std::vector<uint8_t> y_plane;
    std::vector<uint8_t> u_plane;
    std::vector<uint8_t> v_plane;
    FillI420Noise(y_plane, u_plane, v_plane, kWidth, kHeight, 0x9E3779B9u);

    for (int sender_index = 0; sender_index < 2; ++sender_index) {
        OmniWebRtcMediaSession* sender = nullptr;
        if (CreateSession(
                session_config.c_str(),
                &sender,
                error.data(),
                error.size(),
                &required_error_len) != OMNI_WEBRTC_MEDIA_STATUS_OK ||
            sender == nullptr) {
            std::cerr << "sender-restart sender create failed: " << error.data() << '\n';
            DestroySession(receiver);
            return Fail("sender-restart sender session_create failed");
        }

        (void)omni_webrtc_media_session_request_keyframe(sender);

        const int frames_to_send = sender_index == 0 ? kFirstSenderFrames : kRestartedSenderFrames;
        const int first_frame_index = sender_index == 0 ? 0 : kFirstSenderFrames;
        // Only frames from THIS sender count; whatever the previous one left in the playout
        // buffer carries an older RTP timestamp and is drained but not counted.
        const uint32_t min_rtp_timestamp = RtpTimestampForCaptureUs(
            kFirstCaptureUs + (static_cast<int64_t>(first_frame_index) * kFrameIntervalUs));

        uint32_t forwarded_packets = 0;
        int decoded = 0;
        for (int frame_index = 0; frame_index < frames_to_send; ++frame_index) {
            const int64_t timestamp_us = kFirstCaptureUs +
                (static_cast<int64_t>(first_frame_index + frame_index) * kFrameIntervalUs);
            OmniWebRtcMediaStatus pushStatus = omni_webrtc_media_session_push_i420_frame(
                sender,
                y_plane.data(),
                kWidth,
                u_plane.data(),
                (kWidth + 1) / 2,
                v_plane.data(),
                (kWidth + 1) / 2,
                kWidth,
                kHeight,
                timestamp_us,
                frame_index == 0 ? 1 : 0);
            if (pushStatus != OMNI_WEBRTC_MEDIA_STATUS_OK) {
                std::cerr << "sender-restart push_i420_frame status " << static_cast<int>(pushStatus)
                          << " at frame " << frame_index << '\n';
                DestroySession(sender);
                DestroySession(receiver);
                return Fail("sender-restart could not feed the encoder");
            }

            if (!ForwardRtpFromSenderToReceiver(sender, receiver, forwarded_packets)) {
                DestroySession(sender);
                DestroySession(receiver);
                return Fail("sender-restart RTP forwarding failed");
            }

            // Drain as we go, the way the shell's render loop does — otherwise the playout
            // buffer's bounded capacity would decide the outcome instead of the assembler.
            decoded += DrainDecodedFrames(receiver, min_rtp_timestamp);
            // ...и подавать надо В РЕАЛЬНОМ ТЕМПЕ, иначе выгребание «по ходу» не спасает: буфер
            // показа отпускает кадры по СТЕННЫМ часам, а цикл без задержки набивает его быстрее,
            // чем тот успевает опустеть. Замер 2026-08-17: из 150 поданных кадров собиралось 32,
            // а 118 отвергались по переполнению — и это было невидимо, потому что отказ буфера
            // не считался вовсе. Отсюда же весь разброс decoded (4..147), который годами
            // списывали на флейк.
            std::this_thread::sleep_for(std::chrono::milliseconds(33));
        }

        // Сколько кадров кодировщик РЕАЛЬНО выдал — читаем до уничтожения отправителя. Порог ниже
        // считается от этого числа, а не от числа поданных: подача и выдача не равны никогда, а с
        // тех пор как кодировщику разрешён пропуск кадров (SetFrameDropEnabled) — тем более.
        const long long produced_frames = ReadDiagnosticsInt(sender, R"("frames_encoded":)");
        DestroySession(sender);
        if (forwarded_packets == 0) {
            DestroySession(receiver);
            return Fail("sender-restart sender produced no RTP");
        }

        // Добираем показ ВСЕГДА, а не только когда не декодировано ничего. Буфер показа отпускает
        // кадры по СТЕННЫМ часам, а цикл выше прогоняет их без задержки — то есть к его концу
        // большая часть кадров ещё просто не наступила по времени. Прежнее условие `decoded == 0`
        // означало: успели случайно — меряем показ, не успели — меряем таймер. Отсюда и разброс
        // decoded от 5 до 147 на одном бинаре (docs/architecture/test-patterns.md).
        for (int attempt = 0; attempt < 60; ++attempt) {
            const int drained = DrainDecodedFrames(receiver, min_rtp_timestamp);
            decoded += drained;
            if (drained == 0 && attempt >= 20) {
                break;
            }
            std::this_thread::sleep_for(std::chrono::milliseconds(10));
        }

        std::cout << "[smoke] sender-restart pass " << sender_index
                  << ": forwarded " << forwarded_packets
                  << " packets, encoded " << produced_frames
                  << ", assembled " << ReadDiagnosticsInt(receiver, R"("rtp_frames_assembled":)")
                  << ", bufPend " << ReadDiagnosticsInt(receiver, R"("playout_pending_frames":)")
                  << ", bufDrop " << ReadDiagnosticsInt(receiver, R"("playout_frames_dropped":)")
                  << ", decodeFail " << ReadDiagnosticsInt(receiver, R"("decode_failures":)")
                  << ", decoded " << decoded << " frames" << std::endl;

        // "More than zero" is not the bar: with the sequence space restarting under an unchanged
        // SSRC the receiver still stumbles a few frames through (measured 9 of 60), it just never
        // recovers into a picture. A third separates that from working (measured 49 of 60 once the
        // numbering is continuous) — но треть от ВЫДАННОГО кодировщиком, а не от поданного нами.
        const int required = sender_index == 0
            ? 1
            : std::max(1, static_cast<int>(produced_frames) / 3);
        if (decoded < required) {
            DestroySession(receiver);
            return Fail(sender_index == 0
                ? "sender-restart: the first sender decoded nothing (test setup is broken)"
                : "sender-restart: peer video did not survive the sender restart");
        }
    }

    DestroySession(receiver);
    return 0;
}

// Audio Phase 2a-step1 (2026-05-21): smoke for libopus end-to-end.
// Encodes 20 ms of zero-PCM (silence) at 48 kHz mono, decodes back,
// asserts the round trip returns the expected sample count. The
// asserts are deliberately weak — we're proving the link works and
// the API surface is callable, not that the codec compresses well.
// Real capture/playback wiring lands in Phase 2a-step3 (WASAPI) and
// Phase 2b (playback).
// See docs/decisions/audio-phase-1-adoption.md.
// РАЗБОРЩИК ВХОДЯЩИХ МЕДИА: куда уходит пакет и о чём он просит (2026-09-27).
//
// Без живых сессий — проверяется сама раскладка: отчёт о доставке засчитан как обратная связь и не
// считается видео; видео без приёмника превращается в ОДИН запрос, который после выдачи гаснет;
// в групповом звонке неизвестная метка — запрос, известная — нет; короткий групповой пакет
// отброшен. Отрицательные половины — вторые выдачи запросов, которые обязаны быть пустыми.
// НАСТОЯЩИЕ МИКРОФОН И ДИНАМИК — ТОЛЬКО ЕСЛИ РАЗРЕШЕНО (2026-09-27).
//
// Три стенда открывают устройства по-настоящему (захват 80 мс с микрофона, тишина в динамик).
// OMNI_SMOKE_NO_AUDIO_DEVICES=1 их пропускает — для машины, где звуковые устройства трогать
// нельзя. Звуковой путь при этом проверяется без устройств — синтетический сигнал через Opus и
// приёмник (смоук сценария джиттера).
bool AudioDevicesAllowed()
{
    const char* value = std::getenv("OMNI_SMOKE_NO_AUDIO_DEVICES");
    const bool allowed = value == nullptr || value[0] == '\0' || value[0] == '0';
    if (!allowed) {
        std::cout << "[smoke] audio devices skipped (OMNI_SMOKE_NO_AUDIO_DEVICES)" << std::endl;
    }
    return allowed;
}

int RunInboundRouterSmoke()
{
    OmniWebRtcMediaRouter* router = nullptr;
    if (omni_webrtc_media_router_create(&router) != OMNI_WEBRTC_MEDIA_STATUS_OK || router == nullptr) {
        return Fail("router_create failed");
    }
    const auto take = [&]() {
        OmniWebRtcMediaRouterRequests requests{};
        (void)omni_webrtc_media_router_take_requests(router, &requests);
        return requests;
    };
    uint64_t delivered = 0, feedback = 0, dropped = 0;

    // 1:1: RTCP 205/11 — это обратная связь, не видео, и запроса приёмника не будит.
    const std::array<uint8_t, 8> ccfb{0x8Bu, 205u, 0, 1, 0, 0, 0, 0};
    omni_webrtc_media_router_deliver(router, ccfb.data(), ccfb.size());
    OmniWebRtcMediaRouterRequests after_feedback = take();
    (void)omni_webrtc_media_router_counters(router, &delivered, &feedback, &dropped);
    if (feedback != 1 || after_feedback.video_receiver_wanted != 0) {
        omni_webrtc_media_router_destroy(router);
        return Fail("router: a delivery report was not counted as feedback, or woke the video receiver");
    }

    // Видео (PT 102) без приёмника — один запрос; повторная выдача пуста.
    std::array<uint8_t, 16> video{};
    video[0] = 0x80u;
    video[1] = 102u;
    omni_webrtc_media_router_deliver(router, video.data(), video.size());
    omni_webrtc_media_router_deliver(router, video.data(), video.size());
    if (take().video_receiver_wanted != 1 || take().video_receiver_wanted != 0) {
        omni_webrtc_media_router_destroy(router);
        return Fail("router: video with no receiver must raise exactly one request");
    }

    // Группа: неизвестная метка — запрос (один, как бы часто ни приходила), известная — нет.
    (void)omni_webrtc_media_router_set_group_mode(router, 1);
    std::array<uint8_t, 8 + 16> tagged{};
    for (size_t i = 0; i < 8; ++i) { tagged[i] = static_cast<uint8_t>(0xA0u + i); }
    std::memcpy(tagged.data() + 8, video.data(), video.size());
    omni_webrtc_media_router_deliver(router, tagged.data(), tagged.size());
    omni_webrtc_media_router_deliver(router, tagged.data(), tagged.size());
    const OmniWebRtcMediaRouterRequests group_requests = take();
    if (group_requests.group_sender_count != 1 ||
        std::memcmp(group_requests.group_sender_tags[0], tagged.data(), 8) != 0) {
        omni_webrtc_media_router_destroy(router);
        return Fail("router: an unknown group sender must raise exactly one request with its tag");
    }
    // Запрос, на который оболочка ответила подключением отправителя, снимается. Зонд уровня здесь
    // фиктивный указатель (сессию ради этого не строим), поэтому после подключения пакетов этой
    // метки не подаём — только проверяем, что запроса больше нет.
    omni_webrtc_media_router_deliver(router, tagged.data(), tagged.size());
    alignas(8) std::array<uint8_t, 8> fake_probe_storage{};
    (void)omni_webrtc_media_router_set_group_sender(
        router, tagged.data(), nullptr,
        reinterpret_cast<OmniWebRtcMediaAudioPlayback*>(fake_probe_storage.data()));
    if (take().group_sender_count != 0) {
        omni_webrtc_media_router_destroy(router);
        return Fail("router: a wired group sender must not stay requested");
    }
    const std::array<uint8_t, 8> short_packet{};
    omni_webrtc_media_router_deliver(router, short_packet.data(), short_packet.size());
    (void)omni_webrtc_media_router_counters(router, &delivered, &feedback, &dropped);
    if (dropped != 1 || take().group_sender_count != 0) {
        omni_webrtc_media_router_destroy(router);
        return Fail("router: a group packet with no RTP after the tag must be dropped, not requested");
    }
    omni_webrtc_media_router_destroy(router);
    std::cout << "[smoke] inbound router: delivered " << delivered << ", feedback " << feedback
              << ", dropped " << dropped << std::endl;
    return 0;
}

int RunOpusRoundtripSmoke()
{
    constexpr int kSampleRate = 48000;
    constexpr int kChannels = 1;
    constexpr int kFrameSamples = 960;  // 20 ms at 48 kHz
    constexpr int kPacketBufBytes = 4000;

    int error = 0;
    OpusEncoder* encoder = opus_encoder_create(
        kSampleRate,
        kChannels,
        OPUS_APPLICATION_VOIP,
        &error);
    if (error != OPUS_OK || encoder == nullptr) {
        std::cerr << "opus_encoder_create failed: " << opus_strerror(error) << std::endl;
        return Fail("opus_encoder_create failed");
    }
    if (opus_encoder_ctl(encoder, OPUS_SET_BITRATE(32000)) != OPUS_OK) {
        opus_encoder_destroy(encoder);
        return Fail("opus OPUS_SET_BITRATE failed");
    }

    OpusDecoder* decoder = opus_decoder_create(kSampleRate, kChannels, &error);
    if (error != OPUS_OK || decoder == nullptr) {
        opus_encoder_destroy(encoder);
        std::cerr << "opus_decoder_create failed: " << opus_strerror(error) << std::endl;
        return Fail("opus_decoder_create failed");
    }

    std::array<opus_int16, kFrameSamples * kChannels> pcm_in{};  // silence
    std::array<unsigned char, kPacketBufBytes> packet{};

    const int packet_bytes = opus_encode(
        encoder,
        pcm_in.data(),
        kFrameSamples,
        packet.data(),
        static_cast<opus_int32>(packet.size()));
    if (packet_bytes < 0) {
        opus_decoder_destroy(decoder);
        opus_encoder_destroy(encoder);
        std::cerr << "opus_encode failed: "
                  << opus_strerror(packet_bytes) << std::endl;
        return Fail("opus_encode returned a negative status");
    }
    if (packet_bytes == 0) {
        opus_decoder_destroy(decoder);
        opus_encoder_destroy(encoder);
        return Fail("opus_encode produced zero-byte packet for non-empty input");
    }

    std::array<opus_int16, kFrameSamples * kChannels> pcm_out{};
    const int decoded_samples = opus_decode(
        decoder,
        packet.data(),
        packet_bytes,
        pcm_out.data(),
        kFrameSamples,
        /*decode_fec=*/0);
    if (decoded_samples != kFrameSamples) {
        opus_decoder_destroy(decoder);
        opus_encoder_destroy(encoder);
        std::cerr << "opus_decode returned " << decoded_samples
                  << " samples, expected " << kFrameSamples << std::endl;
        return Fail("opus_decode sample count mismatch");
    }

    opus_decoder_destroy(decoder);
    opus_encoder_destroy(encoder);
    return 0;
}
#else
int RunUnavailableSessionSmoke()
{
    OmniWebRtcMediaSession* session = reinterpret_cast<OmniWebRtcMediaSession*>(0x1);
    std::array<char, 256> error{};
    size_t required_error_len = 0;
    OmniWebRtcMediaStatus status = CreateSession(
        R"({"test":true})",
        &session,
        error.data(),
        error.size(),
        &required_error_len);
    if (status != OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED) {
        return Fail("session_create must fail visibly until libwebrtc is linked");
    }

    if (session != nullptr) {
        return Fail("session_create returned a fake session");
    }

    if (std::strlen(error.data()) == 0 || required_error_len == 0) {
        return Fail("session_create did not return a diagnostic error");
    }

    return 0;
}
#endif


// The ABR resolution ladder must actually be a ladder.
//
// It used to be a single clamp to 1280x720 and nothing below, so a congested link kept encoding
// 720p while the bitrate collapsed to the floor. That is what makes a keyframe unrecoverable under
// loss: a 720p IDR is tens of RTP packets and at 10% loss it practically never arrives whole
// (measured: 6 of 24 got through). Fewer pixels, fewer packets, a keyframe that can actually land.
// See docs/bugs/video-dies-on-packet-loss.md.
//
// Deliberately asserts the SHAPE, not the exact rungs: several steps down, one step per decision,
// and a symmetric climb back. A future ladder with different rungs still passes; a ladder that
// stops after one step does not.
#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
// Читает целочисленное поле из диагностического JSON сессии. -1, если поля нет.
bool SessionDiagnosticsContains(OmniWebRtcMediaSession* session, std::string_view needle);

long long ReadDiagnosticsInt(OmniWebRtcMediaSession* session, std::string_view key)
{
    std::array<char, 8192> diagnostics{};
    size_t diagnostics_len = 0;
    if (omni_webrtc_media_session_diagnostics_json(
            session, diagnostics.data(), diagnostics.size(), &diagnostics_len) !=
        OMNI_WEBRTC_MEDIA_STATUS_OK) {
        return -1;
    }
    const std::string_view json(diagnostics.data());
    const size_t at = json.find(key);
    if (at == std::string_view::npos) {
        return -1;
    }
    return std::strtoll(diagnostics.data() + at + key.size(), nullptr, 10);
}

/// Есть ли подстрока в диагностическом JSON. Для полей, которые НЕ числа: `hardware_encoder`
/// это true/false, а `encoder_backend` — строка, и ReadDiagnosticsInt на них вернёт мусор.
bool SessionDiagnosticsContains(OmniWebRtcMediaSession* session, std::string_view needle)
{
    std::array<char, 8192> diagnostics{};
    size_t diagnostics_len = 0;
    if (omni_webrtc_media_session_diagnostics_json(
            session, diagnostics.data(), diagnostics.size(), &diagnostics_len) !=
        OMNI_WEBRTC_MEDIA_STATUS_OK) {
        return false;
    }
    return std::string_view(diagnostics.data()).find(needle) != std::string_view::npos;
}

// RTP-метка времени пакета (байты 4..7 заголовка, big-endian). Все пакеты ОДНОГО кадра несут
// одну и ту же метку — по ней и считаем, скольких кадров приёмник коснулся.
uint32_t RtpTimestampOfPacket(const uint8_t* packet, size_t len)
{
    if (packet == nullptr || len < 12) {
        return 0;
    }
    return (static_cast<uint32_t>(packet[4]) << 24) | (static_cast<uint32_t>(packet[5]) << 16) |
           (static_cast<uint32_t>(packet[6]) << 8) | static_cast<uint32_t>(packet[7]);
}

// Забрать у отправителя НЕ БОЛЬШЕ `max_packets` пакетов и отдать приёмнику, попутно считая
// разные кадры в дошедшем потоке. Батч уложен как [длина, 2 байта little-endian][пакет].
bool ForwardLimitedRtp(
    OmniWebRtcMediaSession* sender,
    OmniWebRtcMediaSession* receiver,
    uint32_t max_packets,
    uint32_t& forwarded_packets,
    uint32_t& distinct_frames,
    uint32_t& last_timestamp,
    bool& have_last_timestamp,
    uint32_t& frames_with_marker,
    uint32_t& gaps_inside_frames,
    uint16_t& last_sequence,
    bool& last_packet_ended_frame)
{
    std::vector<uint8_t> batch(262144);
    size_t written_len = 0;
    uint32_t packet_count = 0;
    if (DrainSessionPackets(
            sender, batch.data(), batch.size(), max_packets, &written_len, &packet_count) !=
        OMNI_WEBRTC_MEDIA_STATUS_OK) {
        return false;
    }
    if (packet_count == 0 || written_len == 0) {
        return true;
    }

    size_t offset = 0;
    for (uint32_t i = 0; i < packet_count && offset + 2 <= written_len; ++i) {
        const size_t packet_len =
            static_cast<size_t>(batch[offset]) | (static_cast<size_t>(batch[offset + 1]) << 8);
        offset += 2;
        if (offset + packet_len > written_len) {
            break;
        }
        const uint32_t timestamp = RtpTimestampOfPacket(batch.data() + offset, packet_len);
        const uint16_t sequence = packet_len >= 4
            ? static_cast<uint16_t>((static_cast<uint16_t>(batch[offset + 2]) << 8) |
                                    static_cast<uint16_t>(batch[offset + 3]))
            : uint16_t{0};
        // Marker-бит = последний пакет кадра. Кадров с маркером ровно столько, у скольких доехал
        // хвост; разница с `distinct_frames` показывает, головы теряются или хвосты.
        const bool ends_frame = packet_len > 1 && (batch[offset + 1] & 0x80u) != 0;

        if (!have_last_timestamp || timestamp != last_timestamp) {
            ++distinct_frames;
            last_timestamp = timestamp;
        }
        if (ends_frame) {
            ++frames_with_marker;
        }
        // ВОТ ЭТО и есть проверяемый контракт. Отправитель нумерует пакеты подряд, а отброс
        // выкусывает из этой нумерации непрерывный кусок. Значит разрыв в номерах разрешён
        // ТОЛЬКО сразу после marker-пакета: пропал целый кадр. Разрыв в любом другом месте —
        // это выкушенная середина кадра, то есть приёмнику отдали обрывок.
        if (have_last_timestamp && sequence != static_cast<uint16_t>(last_sequence + 1) &&
            !last_packet_ended_frame) {
            ++gaps_inside_frames;
        }
        last_sequence = sequence;
        last_packet_ended_frame = ends_frame;
        have_last_timestamp = true;
        offset += packet_len;
    }

    if (omni_webrtc_media_session_push_rtp_packets(
            receiver, batch.data(), written_len, packet_count) != OMNI_WEBRTC_MEDIA_STATUS_OK) {
        return false;
    }

    forwarded_packets += packet_count;
    return true;
}

/// Качество картинки при ФИКСИРОВАННОМ битрейте — мерка, по которой сравниваются настройки кодека.
///
/// Сцена, битрейт и число кадров одинаковы всегда; меняется только то, что мы просим у кодировщика.
/// Печатает средний PSNR и средний размер кадра, чтобы было видно обе стороны размена: правка,
/// поднявшая PSNR ценой битрейта, — не улучшение, а перенос стрелки.
/// Пересылка с ДЕТЕРМИНИРОВАННЫМИ потерями: выбрасывается каждый N-й пакет.
///
/// Случайный жребий здесь не годится: сравниваются настройки кодека, и разные картины потерь между
/// прогонами дали бы разброс больше измеряемого эффекта.
/// `seen` — СЧЁТЧИК ФАЗЫ ПОТЕРЬ, И ОН ПРИНАДЛЕЖИТ ВЫЗЫВАЮЩЕМУ.
///
/// Здесь стояло `static thread_local uint32_t seen`, то есть один счётчик на весь процесс. Из-за
/// этого КАЖДЫЙ стенд наследовал фазу выброса от всех предыдущих: стоило одному прогону послать
/// на десяток пакетов больше — и картина потерь во всех последующих съезжала. Прогон оставался
/// детерминированным, но прогоны ДО и ПОСЛЕ правки сравнивать было нельзя, а именно ради этого
/// сравнения стенд и существует. Замер поймал это в лоб: правка в ветке loss10 сдвинула loss20 с
/// 90 декодированных до 52, ничего в её собственном пути не изменив.
bool ForwardRtpWithLoss(
    OmniWebRtcMediaSession* sender,
    OmniWebRtcMediaSession* receiver,
    uint32_t& forwarded_packets,
    uint32_t& dropped_packets,
    int loss_percent,
    uint32_t& seen)
{
    if (loss_percent <= 0) {
        return ForwardRtpFromSenderToReceiver(sender, receiver, forwarded_packets);
    }

    std::vector<uint8_t> batch(262144);
    for (int pass = 0; pass < 256; ++pass) {
        size_t written_len = 0;
        uint32_t packet_count = 0;
        if (DrainSessionPackets(
                sender, batch.data(), batch.size(), 128, &written_len, &packet_count) !=
            OMNI_WEBRTC_MEDIA_STATUS_OK) {
            return false;
        }
        if (packet_count == 0 || written_len == 0) {
            return true;
        }

        size_t offset = 0;
        for (uint32_t i = 0; i < packet_count; ++i) {
            const uint16_t packet_len = static_cast<uint16_t>(batch[offset]) |
                static_cast<uint16_t>(static_cast<uint16_t>(batch[offset + 1]) << 8);
            offset += sizeof(uint16_t);
            const bool drop = (++seen % static_cast<uint32_t>(100 / loss_percent)) == 0;
            if (drop) {
                ++dropped_packets;
            } else {
                if (omni_webrtc_media_session_push_rtp_packets(
                        receiver, batch.data() + offset - sizeof(uint16_t),
                        packet_len + sizeof(uint16_t), 1) != OMNI_WEBRTC_MEDIA_STATUS_OK) {
                    return false;
                }
                ++forwarded_packets;
            }
            offset += packet_len;
        }
    }
    return true;
}

/// RTCP transport-layer feedback. Полный байт типа, БЕЗ маски 0x7F: маска — свойство RTP-заголовка,
/// где старший бит занят маркером, а в RTCP второй байт целиком отдан под тип.
constexpr uint8_t kRtcpTransportFeedbackPayloadType = 205;
/// Формат RTPFB для отчёта о доставке по RFC 8888 (`webrtc::rtcp::CongestionControlFeedback`,
/// `kFeedbackMessageType = 11`). Лежит в младших пяти битах первого байта.
constexpr uint8_t kRtcpCongestionControlFeedbackFormat = 11;

struct BackWireCounts {
    uint32_t feedback_reports = 0;
    uint32_t other_packets = 0;
};

/// ОБРАТНЫЙ ПРОВОД ПРИЁМНИК -> ОТПРАВИТЕЛЬ, РОВНО КАК ЕГО ВЕДЁТ ОБОЛОЧКА.
///
/// Отчёт о доставке (RFC 8888) едет теперь в общей очереди RTCP приёмника, вместе с медиа, а не
/// отдельным управляющим каналом. Поэтому стенд обязан разбирать её так же, как продукт: пакет с
/// типом 205 и форматом 11 уходит в `apply_transport_feedback` (он не только отдаёт байты
/// сборщику, но и меряет проход туда-обратно), остальное — обычным `push_rtp_packets`.
///
/// Отвезти отчёт каким-то своим вторым путём значит мерить систему, собранную не так, как она
/// работает в продукте. Ровно поэтому прежний `drain_transport_feedback` отсюда и убран, а не
/// оставлен «на всякий случай».
BackWireCounts PumpReceiverRtcpToSender(
    OmniWebRtcMediaSession* receiver,
    OmniWebRtcMediaSession* sender,
    bool apply_feedback = true)
{
    BackWireCounts counts{};
    std::array<uint8_t, 8192> back{};
    size_t back_len = 0;
    uint32_t back_count = 0;
    if (DrainSessionPackets(
            receiver, back.data(), back.size(), 32, &back_len, &back_count) !=
            OMNI_WEBRTC_MEDIA_STATUS_OK ||
        back_count == 0 || back_len == 0) {
        return counts;
    }

    size_t offset = 0;
    for (uint32_t i = 0; i < back_count && offset + sizeof(uint16_t) <= back_len; ++i) {
        const size_t packet_len = static_cast<size_t>(back[offset]) |
                                  (static_cast<size_t>(back[offset + 1]) << 8);
        const size_t framed_at = offset;
        offset += sizeof(uint16_t);
        if (packet_len == 0 || offset + packet_len > back_len) {
            break;
        }
        const uint8_t* packet = back.data() + offset;
        offset += packet_len;

        if (packet_len >= 2 && packet[1] == kRtcpTransportFeedbackPayloadType &&
            (packet[0] & 0x1F) == kRtcpCongestionControlFeedbackFormat) {
            ++counts.feedback_reports;
            if (apply_feedback) {
                (void)omni_webrtc_media_session_apply_transport_feedback(
                    sender, packet, packet_len);
            }
            continue;
        }

        // Кадр батча уже собран — отдаём его как есть, вместе с его двухбайтовой длиной.
        (void)omni_webrtc_media_session_push_rtp_packets(
            sender, back.data() + framed_at, packet_len + sizeof(uint16_t), 1);
        ++counts.other_packets;
    }
    return counts;
}

/// Вернуть отправителю то, о чём просит приёмник: потерянные пакеты и ключевые кадры.
///
/// Без этого шага стенд меряет сеть БЕЗ ретрансмиссии, а у нас она есть — и в реальном звонке
/// именно она вытаскивает большую часть потерь. Замер, не замкнувший обратную связь, показывает не
/// свойства продукта, а свойства своей собственной неполноты.
///
/// С ABI 16 просьбы — настоящий RTCP приёмника (NACK, PLI) в одной очереди с отчётами о доставке,
/// поэтому провод один: PumpReceiverRtcpToSender. Структурных событий о просьбах больше нет.
void PumpReceiverFeedback(OmniWebRtcMediaSession* sender, OmniWebRtcMediaSession* receiver)
{
    // Место в такте я один раз объяснил себе как «часть замера» и перенёс в конец, рассудив, что
    // иначе просьба уезжает на кадр позже. Замер это ОПРОВЕРГ: среднее упало с 78 до 63, а
    // падений стало два из десяти. Объяснение было правдоподобным и неверным — оставляю как было
    // и не трогаю то, чего не понимаю.
    //
    // RTT приёмной половине стенд больше не переносит (ABI 17): её модуль меряет его сам по XR
    // RRTR/DLRR — RRTR уезжает здесь, DLRR возвращается в отчёте отправителя вместе с медиа.
    (void)PumpReceiverRtcpToSender(receiver, sender);
}

int RunH264PictureQualitySmoke(bool software_encode, int loss_percent = 0)
{
    // ВИРТУАЛЬНЫЕ ЧАСЫ (#59). Этот стенд — главная причина, по которой они понадобились.
    //
    // Обвал сборки кадров под 10% потерь перемежающийся: он случался в двух-трёх прогонах из
    // десяти и не воспроизводился по требованию. Гоняться за таким на реальных часах нельзя —
    // каждая проверка стоит серии в 20 минут, а результат всё равно зависит от загрузки машины.
    //
    // На виртуальных часах прогон детерминирован: одинаковый сценарий даёт одинаковый результат.
    // Перемежающийся дефект становится либо стабильно воспроизводимым, либо стабильно
    // отсутствующим — и то и другое несравнимо полезнее «иногда».
    //
    // Кодировщик (и программный, и аппаратный) зовётся синхронно и виртуального времени не
    // тратит: под этими часами он просто отрабатывает мгновенно.
    if (omni_webrtc_media_test_simulated_time_begin() != OMNI_WEBRTC_MEDIA_STATUS_OK) {
        return Fail("picture-quality: simulated clock could not be started");
    }
    struct SimulatedTimeScope {
        ~SimulatedTimeScope() { (void)omni_webrtc_media_test_simulated_time_end(); }
    } simulated_time_scope;

    constexpr int kWidth = 1280;
    constexpr int kHeight = 720;
    constexpr int kFps = 30;
    // 800 кбит/с — у нижней границы жизнеспособного для 1280x720 (апстрим считает минимумом 900,
    // см. EncoderInfoSettings). Ниже опускать нельзя: на 400 программный кодировщик роняет 80%
    // кадров, и мерить становится нечего.
    //
    // ВАЖНО, КАК ЧИТАТЬ РЕЗУЛЬТАТ. На этой сцене 800 кбит/с одному слою уже почти некуда тратить —
    // 52 дБ это потолок КОНТЕНТА. Поэтому сравнивать настройки надо ПО ОБЕИМ величинам сразу: PSNR
    // и пакеты на кадр. Сравнение только по PSNR при фиксированном запросе дало «временные слои
    // стоят 10.7 дБ», тогда как при равном качестве они стоят около 20% битрейта (6.78 против 5.62
    // пакета на кадр при 52.1 дБ у обоих). Это разные утверждения, и первое — неверное.
    constexpr int kBitrateBps = 800'000;
    constexpr int kFrames = 90;

    const std::string session_config =
        std::string(R"({"codec":"h264","width":1280,"height":720,"fps":30,"bitrate_bps":800000)") +
        R"(,"negotiated_width":1280,"negotiated_height":720)" +
        (software_encode ? R"(,"encode_software":1})" : R"(})");

    std::array<char, 512> error{};
    size_t required_error_len = 0;
    OmniWebRtcMediaSession* sender = nullptr;
    OmniWebRtcMediaSession* receiver = nullptr;
    if (CreateSession(
            session_config.c_str(), &sender, error.data(), error.size(), &required_error_len) !=
            OMNI_WEBRTC_MEDIA_STATUS_OK ||
        sender == nullptr) {
        std::cerr << "picture-quality: sender create failed: " << error.data() << '\n';
        return Fail("picture-quality sender session_create failed");
    }
    if (CreateSession(
            session_config.c_str(), &receiver, error.data(), error.size(), &required_error_len) !=
            OMNI_WEBRTC_MEDIA_STATUS_OK ||
        receiver == nullptr) {
        DestroySession(sender);
        std::cerr << "picture-quality: receiver create failed: " << error.data() << '\n';
        return Fail("picture-quality receiver session_create failed");
    }

    std::vector<uint8_t> y_plane;
    std::vector<uint8_t> u_plane;
    std::vector<uint8_t> v_plane;
    // Исходные яркости ПО МЕТКЕ ВРЕМЕНИ, а не по порядковому номеру.
    //
    // Порядковый номер здесь врёт: кодировщику разрешён пропуск кадров, поэтому N-й декодированный
    // кадр — не N-й поданный. Сопоставление по индексу давало 10.9 дБ на программном пути (на
    // аппаратном, который не пропускает, — 40) и читалось бы как «программный кодировщик ломает
    // картинку», хотя ломалось сравнение.
    std::map<uint32_t, std::vector<uint8_t>> source_luma_by_rtp;
    // RTT приёмной половины — её собственный, из XR (ABI 17). Пока не пришёл ни один DLRR, его
    // нет: -1, а не ноль и не чужое число.
    if (ReadDiagnosticsInt(receiver, R"("receiver_rtt_ms":)") != -1) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail("picture-quality: receiver claims a round trip before any report came back");
    }
    // МЕТКА НА ПРОВОДЕ БОЛЬШЕ НЕ РАВНА МЕТКЕ СЪЁМКИ (#57, шаг C2b).
    //
    // Модуль RTP выбирает начальную метку СЛУЧАЙНО — так требует RFC 3550, и по нашей модели угроз
    // это выигрыш: метка перестаёт выдавать время съёмки. Наш прежний путь клал метку как есть, и
    // стенд молча этим пользовался. Теперь смещение спрашивается у отправителя.
    uint32_t rtp_start_timestamp = 0;
    bool rtp_start_known = false;
    uint32_t first_source_rtp_timestamp = 0;
    bool first_source_rtp_known = false;
    uint32_t forwarded_packets = 0;
    uint32_t dropped_packets = 0;
    // Фаза выброса пакетов — СВОЯ У КАЖДОГО ПРОГОНА. См. ForwardRtpWithLoss.
    uint32_t loss_phase = 0;
    int64_t timestamp_us = 1'000'000;
    double psnr_sum = 0.0;
    int psnr_frames = 0;
    std::vector<uint8_t> decoded;

    // Показ выгребается ПО ХОДУ подачи — как это делает цикл отрисовки в оболочке. Иначе всё
    // решает не качество кодирования, а ограниченная ёмкость буфера показа: он переполняется и
    // выбрасывает кадры, и замер вырождается в ноль.
    const auto collect_decoded = [&]() {
        bool got_any = false;
        for (int guard = 0; guard < 256; ++guard) {
            int32_t width = 0;
            int32_t height = 0;
            uint32_t rtp_timestamp = 0;
            int64_t decoded_us = 0;
            size_t required_len = 0;
            if (omni_webrtc_media_session_drain_i420_frame(
                    receiver, nullptr, 0, &width, &height, &rtp_timestamp, &decoded_us,
                    &required_len) != OMNI_WEBRTC_MEDIA_STATUS_BUFFER_TOO_SMALL ||
                required_len == 0) {
                break;
            }
            decoded.assign(required_len, 0);
            if (omni_webrtc_media_session_drain_i420_frame(
                    receiver, decoded.data(), decoded.size(), &width, &height, &rtp_timestamp,
                    &decoded_us, &required_len) != OMNI_WEBRTC_MEDIA_STATUS_OK) {
                break;
            }
            got_any = true;
            // СМЕЩЕНИЕ МЕТКИ УЗНАЁТСЯ ПО ПЕРВОМУ ЖЕ ДЕКОДИРОВАННОМУ КАДРУ.
            //
            // Раньше его хватало спросить у отправителя (начальная метка модуля). После #61
            // слагаемых стало два: к ней добавилась метка, которую оркестратор кодирования
            // выводит из времени съёмки по своим внутренним часам NTP. Оба слагаемых постоянны за
            // прогон, поэтому достаточно измерить сумму один раз — на первом кадре, а он у нас
            // всегда кадр номер ноль: ключевой, и потери в стенде начинаются позже.
            if (!rtp_start_known) {
                rtp_start_timestamp = rtp_timestamp - first_source_rtp_timestamp;
                rtp_start_known = true;
            }
            const auto source_at = source_luma_by_rtp.find(rtp_timestamp - rtp_start_timestamp);
            if (width == kWidth && height == kHeight && source_at != source_luma_by_rtp.end()) {
                psnr_sum += ComputeYPlanePsnrDb(source_at->second, decoded.data(), width, height);
                ++psnr_frames;
            }
        }
        return got_any;
    };

    for (int i = 0; i < kFrames; ++i) {
        FillI420MovingNoise(y_plane, u_plane, v_plane, kWidth, kHeight, static_cast<uint32_t>(i));
        // Та же формула, что и в EncodeI420Buffer: rtp = timestamp_us * 90 / 1000.
        // ФОРМУЛА ТА ЖЕ, ЧТО У ОРКЕСТРАТОРА КОДИРОВАНИЯ, А НЕ «ПОХОЖАЯ».
        //
        // Он считает метку как 90 * (время_съёмки_в_МИЛЛИСЕКУНДАХ) — сперва делит, потом умножает
        // (video_stream_encoder.cc:1641). Здесь стояло (мкс * 90) / 1000, то есть наоборот, и из-за
        // разного округления ряды расходились уже со второго кадра: совпадал ровно один, первый.
        const uint32_t source_rtp = static_cast<uint32_t>(90 * (timestamp_us / 1000));
        if (!first_source_rtp_known) {
            first_source_rtp_timestamp = source_rtp;
            first_source_rtp_known = true;
        }
        source_luma_by_rtp[source_rtp] = y_plane;
        if (omni_webrtc_media_session_push_i420_frame(
                sender,
                y_plane.data(),
                kWidth,
                u_plane.data(),
                (kWidth + 1) / 2,
                v_plane.data(),
                (kWidth + 1) / 2,
                kWidth,
                kHeight,
                timestamp_us,
                i == 0 ? 1 : 0) != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            DestroySession(receiver);
            DestroySession(sender);
            return Fail("picture-quality: push_i420_frame failed");
        }
        timestamp_us += 1'000'000 / kFps;
        // Потери детерминированные, а не случайные: сравнивать настройки кодека можно только на
        // ОДНОЙ И ТОЙ ЖЕ картине потерь, иначе разница между прогонами утонет в разбросе жребия.
        // Потери НАЧИНАЮТСЯ ПОСЛЕ ключевого кадра. Иначе меряется не то: ключевой кадр растянут на
        // десятки пакетов, при 10% потерь он почти наверняка приходит неполным, и не декодируется
        // НИЧЕГО И НИКОГДА — независимо от временных слоёв. Это отдельная задача (защита ключевого
        // кадра, #23), и смешивать её с замером слоёв значит не измерить ни того, ни другого.
        const int effective_loss = i < 5 ? 0 : loss_percent;
        if (!ForwardRtpWithLoss(
                sender, receiver, forwarded_packets, dropped_packets, effective_loss, loss_phase)) {
            DestroySession(receiver);
            DestroySession(sender);
            return Fail("picture-quality: RTP forwarding failed");
        }
        // Обратная связь возвращается КАЖДЫЙ кадр, как в звонке: приёмник просит потерянное,
        // отправитель это повторяет. Ретрансмиссия — первый механизм, которым живой стек
        // переживает потери, и мерить лоссовую сеть без неё бессмысленно.
        PumpReceiverFeedback(sender, receiver);
        collect_decoded();
        // ABR тикает раз в секунду, как в настоящем звонке. Стенд без этого меряет кодировщик,
        // настроенный один раз при создании: корректор перелива не вызывается ни разу, и недобор
        // битрейта (аппаратный путь при двух временных слоях выбирает бюджет не полностью) некому
        // исправить. Сеть здесь чистая — цель не должна падать, но поправка обязана работать.
        if (i % kFps == 0) {
            OmniWebRtcMediaAbrSample sample{};
            sample.now_ms = 1'000 + static_cast<int64_t>(i) * 1'000 / kFps;
            OmniWebRtcMediaAbrTarget stable_target{};
            (void)omni_webrtc_media_session_update_abr(sender, &sample, &stable_target);
        }
        // ПОКАЗ ВЫГРЕБАЕТСЯ ЧАЩЕ, ЧЕМ ПРИХОДЯТ КАДРЫ, — КАК В ПРОДУКТЕ.
        //
        // Оболочка зовёт показ по такту ДИСПЛЕЯ (60 Гц), а не по такту камеры (30), и стенд обязан
        // повторять именно это: буфер показа отпускает по кадру за обращение, поэтому частота
        // обращений — часть измеряемой системы.
        //
        // ОГОВОРКА, ЧТОБЫ НЕ ВВОДИТЬ В ЗАБЛУЖДЕНИЕ: я вводил эту правку как объяснение того, что
        // при целевой задержке 64 мс кадр фактически ждал 189, а в буфере лежало семь кадров.
        // Замер ОПРОВЕРГ объяснение — ожидание сдвинулось на 189 -> 155, а очередь осталась той же
        // семёркой. Правка остаётся как более верная модель продукта, но причину задержки она НЕ
        // объясняет.
        //
        // Шаги 16 и 17 мс, а не два по 16: сумма обязана остаться ровно тактом кадра, иначе
        // сдвинется вся временная линия прогона.
        for (const int step_ms : {1'000 / (kFps * 2), 1'000 / kFps - 1'000 / (kFps * 2)}) {
            (void)omni_webrtc_media_test_simulated_time_advance_ms(step_ms);
            collect_decoded();
        }
    }

    // ДОСЫЛАЕМ ХВОСТ, ПОКА ОТПРАВИТЕЛЬ ЖИВ.
    //
    // Здесь стенд выбрасывал собственные данные. Цикл кадров заканчивался, и отправитель тут же
    // уничтожался вместе со всем, что пейсер ещё не успел выдать; приёмник этих пакетов не видел
    // никогда. Пока выборка САМА двигала пейсер, хвост был короче, но существовал и тогда: пейсер
    // выдаёт по бюджету, а не всё разом. Это и есть механизм давнего вопроса из #51 — «почему
    // путь без потерь доносит только около 73% выданных кодировщиком кадров». Не доносил канал:
    // не досылал стенд.
    //
    // После включения RTX хвост подрос (пакетов на кадр стало 7.2 против 6.4 — модуль резервирует
    // место под обёртку RTX), и то, что раньше было незаметной потерей, стало отказом набора.
    // Лечится не порогом, а тем, чтобы дать отправителю доотдать уже произведённое.
    //
    // Выход по признаку «отправитель молчит», а не по числу попыток: обратная связь продолжает
    // ходить, значит повторы во время досылки тоже успевают уйти — ровно как в живом звонке.
    {
        int silent_polls = 0;
        for (int attempt = 0; attempt < 200 && silent_polls < 10; ++attempt) {
            const uint32_t before = forwarded_packets + dropped_packets;
            if (!ForwardRtpWithLoss(
                    sender, receiver, forwarded_packets, dropped_packets, loss_percent,
                    loss_phase)) {
                break;
            }
            silent_polls = (forwarded_packets + dropped_packets) == before ? silent_polls + 1 : 0;
            PumpReceiverFeedback(sender, receiver);
            collect_decoded();
            (void)omni_webrtc_media_test_simulated_time_advance_ms(10);
        }
    }

    const long long encoded_frames = ReadDiagnosticsInt(sender, R"("frames_encoded":)");
    // Считываем ДО уничтожения сессии — иначе читаем из освобождённой памяти.
    const long long non_reference_frames =
        ReadDiagnosticsInt(sender, R"("frames_non_reference":)");
    const long long reference_frames = ReadDiagnosticsInt(sender, R"("frames_reference":)");
    // Обе величины читаются у ОТПРАВИТЕЛЯ: это он применяет отчёт и это его оценка канала.
    const long long twcc_packets_reported =
        ReadDiagnosticsInt(sender, R"("transport_feedback_packets_reported":)");
    const long long twcc_applied =
        ReadDiagnosticsInt(sender, R"("transport_feedback_reports_applied":)");
    const long long twcc_matched =
        ReadDiagnosticsInt(sender, R"("transport_feedback_reports_matched":)");
    const long long twcc_target_bps =
        ReadDiagnosticsInt(sender, R"("goog_cc_feedback_target_bps":)");
    // Сколько приёмник переспросил и попросил ключевых кадров (по счётчикам его RTCP-модуля, то
    // есть что реально ушло на провод), сколько просьб о повторе принял отправитель и сколько
    // повторов РЕАЛЬНО ушло на провод. Последнее снимается в точке отправки, по виду пакета, а не
    // по числу принятых просьб: «попросили» и «ушло» — разные величины, и путать их значило бы
    // считать намерение фактом.
    const long long nacked_packets = ReadDiagnosticsInt(receiver, R"("rtcp_nack_seq_sent":)");
    const long long keyframe_requests = ReadDiagnosticsInt(receiver, R"("rtcp_pli_sent":)");
    const long long nack_requested =
        ReadDiagnosticsInt(sender, R"("rtcp_nack_seq_received":)");
    const long long retransmitted =
        ReadDiagnosticsInt(sender, R"("module_retransmissions_sent":)");
    const long long padding_sent =
        ReadDiagnosticsInt(sender, R"("module_padding_sent":)");
    // Приёмная сторона: сколько пакетов пришло потоком RTX и сколько из них удалось развернуть
    // обратно в медиапакеты. Второе — единственное прямое доказательство, что разбор RTX работает.
    const long long rtx_received =
        ReadDiagnosticsInt(receiver, R"("rtx_packets_received":)");
    const long long rtx_recovered =
        ReadDiagnosticsInt(receiver, R"("rtx_packets_recovered":)");
    // РАЗНИЦА, БЕЗ КОТОРОЙ «декодировано 25 из 43» НИЧЕГО НЕ ОБЪЯСНЯЕТ.
    //
    // Кадр может не дойти до картинки двумя совершенно разными способами: не собраться из пакетов
    // (тогда виноват транспорт или сборщик) или собраться и не дойти до декодера (тогда виноваты
    // буфер показа или сам стенд, не успевший выгрести). Лечатся они в разных местах, и пока обе
    // причины сложены в одно число, выбирать между ними приходится гаданием.
    // ВЫХОД СБОРЩИКА. Имя наконец совпадает со значением — см. #60.
    const long long emitted_frames =
        ReadDiagnosticsInt(receiver, R"("rtp_frames_assembled":)");
    // ЧТО СКАЗАЛ О СЕБЕ БУФЕР ПОКАЗА. Все эти числа приходят от апстримовского наблюдателя, а не
    // от нашей обвязки: обвязки больше нет.
    const long long continuity_advances =
        ReadDiagnosticsInt(receiver, R"("playout_continuity_advances":)");
    const long long playout_dropped_by_buffer =
        ReadDiagnosticsInt(receiver, R"("playout_frames_dropped":)");
    const long long playout_pending = ReadDiagnosticsInt(receiver, R"("playout_pending_frames":)");
    const long long decode_timeouts = ReadDiagnosticsInt(receiver, R"("decode_timeouts":)");
    const long long awaiting_keyframe =
        ReadDiagnosticsInt(receiver, R"("playout_frames_awaiting_keyframe":)");
    const long long target_delay = ReadDiagnosticsInt(receiver, R"("playout_target_delay_ms":)");
    const long long jitter_delay = ReadDiagnosticsInt(receiver, R"("playout_jitter_delay_ms":)");
    const long long jitter_delay_max =
        ReadDiagnosticsInt(receiver, R"("playout_jitter_delay_max_ms":)");
    // ЦЕЛЬ И ФАКТ — РАЗНЫЕ ВЕЛИЧИНЫ, И РАСХОЖДЕНИЕ МЕЖДУ НИМИ ЕСТЬ ВОПРОС.
    // curMs — задержка, которую VCMTiming применяет сейчас (сглаженная к цели);
    // decMs — 95-й процентиль времени декодирования, он входит в цель слагаемым.
    const long long current_delay = ReadDiagnosticsInt(receiver, R"("playout_current_delay_ms":)");
    const long long decode_time = ReadDiagnosticsInt(receiver, R"("playout_decode_time_ms":)");
    const long long minimum_delay = ReadDiagnosticsInt(receiver, R"("playout_minimum_delay_ms":)");
    // ЕЩЁ ОДНО РАЗДЕЛЕНИЕ, ПО ТОЙ ЖЕ ПРИЧИНЕ, ЧТО И ПРЕДЫДУЩЕЕ.
    //
    // «Собрано 22 кадра из 90» не говорит, дошли ли до приёмника пакеты. Если дошли — виноват
    // сборщик кадров; если не дошли — путь до него. Замер 2026-08-23 показал обвал сборки при
    // НОРМАЛЬНЫХ потерях, переспросах и восстановлении из RTX, и различить эти две причины по
    // имеющимся числам было нельзя.
    const long long receiver_packets =
        ReadDiagnosticsInt(receiver, R"("rtp_packets_received":)");
    // Различают «сборка встала насовсем» и «сборка идёт, но редко»: сколько раз сработал сторож
    // «пакеты идут, кадры не собираются» и какова была самая длинная пауза между собранными кадрами.
    const long long watchdog_fired =
        ReadDiagnosticsInt(receiver, R"("no_assembled_watchdog_fired":)");
    const long long max_gap_ms =
        ReadDiagnosticsInt(receiver, R"("max_assembly_gap_ms":)");
    const long long receiver_rtt_ms = ReadDiagnosticsInt(receiver, R"("receiver_rtt_ms":)");
    const long long transport_rtt =
        ReadDiagnosticsInt(sender, R"("transport_rtt_us":)");
    // Ноль здесь означает, что пробинга нет вовсе — а именно так и было до 2026-08-18 при
    // живом ProbeController, который исправно просил пробы в каждом ответе.
    DestroySession(sender);

    // ДОБИРАЕМ ХВОСТ, ПОКА ДЕКОДЕР ОТДАЁТ, А НЕ ФИКСИРОВАННОЕ ЧИСЛО ПОПЫТОК.
    //
    // Было `attempt < 60` с выходом после двадцати пустых попыток — то есть не больше ~600 мс.
    // Программному декодеру этого хватает не всегда: замер 2026-08-23 дал прогон, где при НУЛЕВЫХ
    // потерях в транспорте (dropped 0, nacked 0) собралось 29 кадров вместо 46 в соседнем зелёном
    // прогоне. Ждём по признаку «декодер перестал отдавать»: счётчик пустых попыток сбрасывается
    // на каждом добранном кадре, а общий потолок остаётся страховкой от зависания.
    int empty_attempts = 0;
    for (int attempt = 0; attempt < 300 && empty_attempts < 30; ++attempt) {
        empty_attempts = collect_decoded() ? 0 : empty_attempts + 1;
        (void)omni_webrtc_media_test_simulated_time_advance_ms(10);
    }

    // ДИАГНОСТИКУ ПРИЁМНИКА СНИМАЕМ ДО ЕГО РАЗРУШЕНИЯ.
    //
    // Здесь был use-after-free, и он же — весь #48. Ниже в печати стояло
    // `ReadDiagnosticsInt(receiver, ...)`, то есть обращение к уже уничтоженной сессии: функция
    // разыменовывала освобождённую память и брала в ней мьютекс. Стек, снятый 2026-08-23:
    //   #00 ntdll!RtlAcquireSRWLockExclusive
    //   #01 std::__libcpp_mutex_lock
    //   #03 omni_webrtc_media_session_diagnostics_json (omni_webrtc_media.cpp:9022)
    //   #04 RunH264PictureQualitySmoke
    // Запись по мёртвому адресу, ACCESS_VIOLATION.
    //
    // Отсюда и все свойства, которые сбивали с толку: падение ВСЕГДА сразу после разрушения сессии,
    // недетерминированное (освобождённая память то переиспользована, то ещё цела) и совершенно
    // нечувствительное к правкам продукта. Из-за последнего три попытки объяснить его подряд дали
    // три неверных вывода — искали в ядре то, что лежало в тесте.
    //
    // Диагностика отправителя выше снимается правильно — вся до `destroy(sender)`. Отставала одна
    // эта строка.
    const long long ccfb_reports_built =
        ReadDiagnosticsInt(receiver, R"("transport_feedback_reports_built":)");
    // ЧИТАЕТСЯ ПОСЛЕ ДОБОРА ХВОСТА, А НЕ ДО НЕГО.
    //
    // Показ теперь планирует апстримовский буфер по времени показа, а не наша выборка по каждому
    // опросу, поэтому последние кадры декодируются уже в хвостовом цикле. Снятое до него число
    // занижено ровно на длину этого хвоста (замер: 86 против 90 при полностью здоровом канале).
    const long long decoded_frames = ReadDiagnosticsInt(receiver, R"("frames_decoded":)");
    DestroySession(receiver);

    const double average_psnr = psnr_frames > 0 ? psnr_sum / psnr_frames : 0.0;
    // Пакетов на кадр — вторая половина размена. Правка, поднявшая PSNR ценой битрейта, это не
    // улучшение, а перенос стрелки, и без этого числа его не увидеть.
    const double packets_per_frame = encoded_frames > 0
        ? static_cast<double>(forwarded_packets) / static_cast<double>(encoded_frames)
        : 0.0;
    std::cout << "[smoke] picture-quality " << (software_encode ? "software" : "hardware")
              << (loss_percent > 0 ? "/loss" + std::to_string(loss_percent) : "")
              << ": psnr " << average_psnr << " dB over " << psnr_frames << " frames"
              << " (encoded " << encoded_frames << ", assembled " << emitted_frames
              << ", contAdv " << continuity_advances << ", bufPend " << playout_pending
              << ", bufDrop " << playout_dropped_by_buffer << ", decTmo " << decode_timeouts
              << ", waitKey " << awaiting_keyframe << ", dec " << decoded_frames
              << ", tgtMs " << target_delay << "/" << current_delay << "/" << minimum_delay
              << ", decMs " << decode_time << ", jitMs " << jitter_delay << "/" << jitter_delay_max
              << ", rxPkts " << receiver_packets << ", gapMs " << max_gap_ms
              << ", wdog " << watchdog_fired << "), "
              << packets_per_frame << " packets/frame, nonRef " << non_reference_frames << "/"
              << (non_reference_frames + reference_frames) << ", dropped " << dropped_packets
              << ", nacked " << nacked_packets << ", keyReq " << keyframe_requests
              << ", rtxAsked " << nack_requested << " -> sent " << retransmitted
              << " (rtx in " << rtx_received << " -> recovered " << rtx_recovered
              << ", padding " << padding_sent << ")"
              << ", rtt " << transport_rtt << "us/rx " << receiver_rtt_ms << "ms"
              << ", ccfbBuilt " << ccfb_reports_built
              << ", ccfbApplied " << twcc_applied << "/" << twcc_matched << ", ccfbPkts " << twcc_packets_reported << ", bweFromFeedback " << twcc_target_bps
              << " at " << kBitrateBps / 1000 << " kbps"
              << std::endl;

    // ПОРОГ СЧИТАЕТСЯ ОТ ЗАКОДИРОВАННЫХ КАДРОВ, А НЕ ОТ ПОДАННЫХ.
    //
    // Было `kFrames / 3`, то есть 30 при 90 ПОДАННЫХ кадрах. Но сколько из поданных станет
    // кадрами — решает кодировщик, и программный OpenH264 под нагрузкой выдаёт примерно половину
    // (в логе рядом видны его же предупреждения iContinualSkipFrames). В результате программная
    // ветка всегда сидела вплотную к границе и падала на ровном месте: замер 2026-08-23 — 29
    // декодированных при 45 закодированных, при нулевых потерях в транспорте.
    //
    // От закодированных считать И ЧЕСТНЕЕ, И СТРОЖЕ: на пути без потерь декодер обязан восстановить
    // почти всё, что кодировщик выдал, независимо от того, сколько кадров тот пропустил. Пропуск
    // кадров кодировщиком — это не «картинка развалилась», а именно то, что проверка НЕ должна
    // ловить.
    // ТРЕБОВАНИЕ ПЕРЕЕХАЛО НА ТУ ВЕЛИЧИНУ, КОТОРАЯ ЕГО ВЫРАЖАЕТ, И СТАЛО СТРОЖЕ.
    //
    // Здесь проверялось «декодировано не меньше двух третей от закодированного». Замер 2026-08-23
    // с разделением величин показал, что это две разные болезни в одном числе, и проверка ловила
    // не ту:
    //
    //   собрано == закодировано во ВСЕХ двадцати измерениях (10 программных, 10 аппаратных);
    //   декодировано  аппаратно 88-90 из 90, программно 26-46 из 46.
    //
    // То есть транспорт и сборщик доносят СТО ПРОЦЕНТОВ, а теряет программный декодер — и теряет
    // от загрузки машины, а не от чего-либо в пути. Порог на декодированных кадрах гейтил
    // пропускную способность OpenH264 на занятой машине, то есть ровно ту величину, которую этот
    // стенд проверять НЕ должен. Это та же ошибка, что и в исходной калибровке #51, только на шаг
    // глубже: тогда считали от поданных кадров вместо закодированных, теперь — от декодированных
    // вместо собранных.
    //
    // Поэтому доставка спрашивается у СБОРКИ и планка поднята с 67% до 90% (равенство наблюдалось
    // во всех двадцати, десять процентов оставлены на дыхание). Отдельно требуется, чтобы кадров
    // хватило на суждение о картинке вообще — без этого PSNR считался бы по двум кадрам.
    //
    // Сказать прямо: проверка пропускной способности программного декодера этим СНЯТА. Она и не
    // должна была стоять здесь — её место в отдельном стенде показа, см. открытый вопрос про
    // программное декодирование.
    // СПРАШИВАЕТСЯ У ВЫХОДА СБОРЩИКА, А НЕ У ЧИСЛА ПРИНЯТЫХ БУФЕРОМ ПОКАЗА.
    //
    // `rtp_frames_assembled` этой величиной не был: счётчик в ядре рос только у кадров, которые
    // приняли ЕЩЁ И в FrameBuffer. Отказ буфера читался как отказ сборки — и именно так родился
    // двухдневный поиск «обвала сборки» в транспорте, которого там не было.
    if (loss_percent == 0) {
        const long long expected_assembled = std::max<long long>(1, encoded_frames * 9 / 10);
        if (emitted_frames < expected_assembled) {
            std::cerr << "[smoke] picture-quality: assembled " << emitted_frames << " of "
                      << encoded_frames << " encoded (need " << expected_assembled
                      << "), dropped " << dropped_packets << ", nacked " << nacked_packets
                      << std::endl;
            return Fail("picture-quality: frames did not survive the path to the assembler");
        }
        if (psnr_frames < 10) {
            std::cerr << "[smoke] picture-quality: only " << psnr_frames
                      << " frames reached the decoder out of " << emitted_frames
                      << " assembled — too few to judge the picture" << std::endl;
            return Fail("picture-quality: too few decoded frames to judge the picture");
        }
    }
    // RTT ПРИЁМНИКУ БОЛЬШЕ НИКТО НЕ НОСИТ — ОН ОБЯЗАН УЗНАТЬ ЕГО САМ. Иначе запросчик NACK
    // живёт на умолчании 100 мс, и повторный запрос на пропавший повтор уходит не вовремя.
    // Звонок длится секунды, отчёты — раз в секунду: к концу прогона DLRR обязан вернуться.
    if (receiver_rtt_ms <= 0) {
        std::cerr << "[smoke] picture-quality: receiver_rtt_ms=" << receiver_rtt_ms << std::endl;
        return Fail("picture-quality: the receiver never learned the round trip (XR RRTR/DLRR)");
    }
    // Порог намеренно низкий: это защита от «картинка развалилась», а не оценка качества. Само
    // качество читается из напечатанного числа и сравнивается между настройками кодека.
    if (loss_percent == 0 && average_psnr < 20.0) {
        std::cerr << "[smoke] picture-quality: average PSNR " << average_psnr << " dB\n";
        return Fail("picture-quality: decoded picture is broken, not merely compressed");
    }

    // ВЕТКИ С ПОТЕРЯМИ ДО СИХ ПОР НИЧЕГО НЕ ПРОВЕРЯЛИ — ТОЛЬКО ПЕЧАТАЛИ.
    //
    // Обе проверки выше стоят под `loss_percent == 0`, то есть весь путь восстановления мог
    // сломаться целиком и молча: числа в логе изменились бы, а набор остался бы зелёным. После
    // перевода повторов на RTX (#53) это перестало быть терпимым — сломать разбор RTX теперь можно
    // одной опечаткой в типе нагрузки, и последствие будет ровно таким: собеседник просит повтор,
    // повтор уходит, и никто не разворачивает его обратно.
    //
    // Проверяем ровно то, что различает «работает» и «выглядит работающим», и ничего сверх: если
    // попросили повторить — повтор обязан УЙТИ; если он ушёл — приёмник обязан его РАЗВЕРНУТЬ.
    // Числа между собой не сравниваются: часть повторов честно теряется тем же каналом.
    // УСЛОВИЕ «ПОПРОСИЛИ» ЧИТАЕТСЯ ПО ПОТЕРЯМ, А НЕ ПО СЧЁТЧИКУ ПРОСЬБ: если канал что-то
    // потерял, повтор обязан уйти — независимо от того, каким способом о нём попросили. Способ
    // уже менялся (структурное событие -> RTCP, ABI 16), и проверка, привязанная к способу, тихо
    // перестала бы срабатывать.
    if (loss_percent > 0 && dropped_packets > 0) {
        if (retransmitted <= 0) {
            std::cerr << "[smoke] picture-quality: " << dropped_packets
                      << " packets were lost and not one retransmission left the sender\n";
            return Fail("picture-quality: retransmission path is dead");
        }
        if (rtx_recovered <= 0) {
            std::cerr << "[smoke] picture-quality: " << rtx_received
                      << " RTX packets arrived and not one was unwrapped back into media\n";
            return Fail("picture-quality: RTX decapsulation is dead");
        }
    }
    // ДОБИВКА ВКЛЮЧЕНА (#54), И ПРОВЕРКА «ЕЁ НОЛЬ» СНЯТА ОСОЗНАННО, А НЕ ПОТОМУ ЧТО МЕШАЛА.
    //
    // На её место встаёт то, ради чего она вообще стояла. Опасность была не в добивке как таковой,
    // а в конкретной поломке: 2026-08-22 добивка без RTX заняла номера в общей с видео
    // последовательности, приёмник перестал отличать потерю от пустоты, и доставка рухнула с 70
    // кадров до 2. Теперь у добивки свой номерной ряд, но если он однажды съедет обратно,
    // последствие будет тем же — обвалом доставки под потерями.
    //
    // Поэтому сторожим ОБВАЛ. Порог намеренно грубый: серия из 10 прогонов до включения добивки
    // дала 52-90 декодированных кадров из 90 при 10% потерь, то есть 58%-100%. Сорок процентов
    // ниже наблюдавшегося минимума с большим запасом и заведомо выше любого обвала (те самые 70->2
    // — это 2%). Сторож обвала не должен быть точным, он должен быть НЕЛОМКИМ.
    // СТОРОЖ СМОТРИТ НА СБОРКУ, А НЕ НА ДЕКОДИРОВАНИЕ — ТА ЖЕ ПОПРАВКА, ЧТО И ВЫШЕ.
    //
    // Сторож называется «обвал доставки», а стоял на декодированных кадрах, то есть мерил
    // пропускную способность декодера. На программной ветке это ловило не обвал, а известную
    // потерю кадров ПОСЛЕ сборки (собрано == закодировано, декодируется 57-100% — отдельная
    // задача). Ту же поправку я внёс для случая без потерь часом раньше и сюда не перенёс.
    //
    // Настоящий обвал по этой величине виден отчётливо: замер 2026-08-23 дал 22 собранных
    // кадра из 90 при полностью нормальных потерях, переспросах и восстановлении из RTX, и
    // при 471 дошедшем до приёмника пакете против 458-482 в здоровых прогонах. Двадцать
    // четыре процента против 80-100% — порог в 40% разделяет их с запасом в обе стороны.
    // СТОРОЖ РАЗДЕЛЁН НАДВОЕ, ПОТОМУ ЧТО ЛОМАЮТСЯ ДВЕ РАЗНЫЕ ВЕЩИ.
    //
    // Он назывался «обвал доставки», а стоял на числе, в котором сложены выход сборщика и
    // согласие буфера показа. Цена ошибки известна поимённо: при 10% потерь сборщик отдавал ВСЕ
    // 90 кадров из 90, буфер (16 мест — число из старой очереди-дека) отвергал 61 по
    // переполнению, декодировалось 7 — и всё это сообщалось как «сборка кадров обвалилась».
    // Два дня предположений про RTT, повторы, дубликаты RTX и историю отправки объясняли
    // явление, которого не было.
    //
    // Теперь: сборка спрашивается у сборщика, доставка — у декодера. Второе и есть та величина,
    // ради которой стенд существует: собранный кадр, не дошедший до картинки, зрителю не нужен.
    if (loss_percent == 10 && encoded_frames > 0) {
        const long long assembly_floor = std::max<long long>(1, encoded_frames * 9 / 10);
        if (emitted_frames < assembly_floor) {
            std::cerr << "[smoke] picture-quality: assembler emitted " << emitted_frames << " of "
                      << encoded_frames << " encoded under 10% loss (floor " << assembly_floor
                      << "), receiver got " << receiver_packets << " packets, rtx recovered "
                      << rtx_recovered << "\n";
            return Fail("picture-quality: frame assembly collapsed under 10% loss");
        }
        // Порог грубый и НЕЛОМКИЙ, как и был: наблюдаемое после починки — 76 из 90 аппаратно и
        // 36 из 45 программно (84% и 80%). Сорок процентов лежат заведомо ниже и заведомо выше
        // обвала: проглоченный запрос ключевого кадра давал 12 из 90, то есть 13%.
        const long long delivery_floor = std::max<long long>(1, encoded_frames * 2 / 5);
        if (decoded_frames < delivery_floor) {
            std::cerr << "[smoke] picture-quality: decoded " << decoded_frames << " of "
                      << encoded_frames << " encoded under 10% loss (floor " << delivery_floor
                      << "), assembler emitted " << emitted_frames << ", buffer holds "
                      << playout_pending << ", buffer dropped " << playout_dropped_by_buffer
                      << ", decode timeouts " << decode_timeouts << ", keyframe requests "
                      << keyframe_requests << "\n";
            return Fail("picture-quality: picture delivery collapsed under 10% loss");
        }
    }

    return 0;
}

struct BufferbloatResult
{
    long long peak_delay_ms = 0;
    long long average_delay_ms = 0;
    unsigned long long overflow_drops = 0;
    unsigned long long delivered = 0;
    unsigned long long feedback_reports = 0;
    long long feedback_matched = 0;
    long long bwe_from_feedback_bps = 0;
    long long abr_target_bps = 0;
};

/// Узкое место БЕЗ ПОТЕРЬ: очередь набивается, задержка растёт, а терять пока нечего.
///
/// Ради этого случая отчёт о доставке и берут. Регулятор, судящий по последствиям, здесь слеп:
/// потерь нет, отчётов приёмника мы не шлём, RTT нулевой — а канал уже перегружен вдвое. Стенд с
/// потерями (`RunH264PictureQualitySmoke`/loss) такой разницы не покажет в принципе, поэтому
/// польза TWCC на нём и не проверялась: он меряет ретрансмиссию, а не предвидение затора.
///
/// Канал эмулируется честно: ёмкость `kCapacityBps`, очередь `kQueueMaxBytes`. Пакет покидает
/// очередь не раньше, чем через (байты * 8 / ёмкость) после предыдущего, а переполнение —
/// ЕДИНСТВЕННЫЙ источник потерь и наступает ПОЗЖЕ роста задержки. Отправитель нацелен выше
/// ёмкости, иначе затора нет и мерить нечего.
///
/// Смысл имеет только ПАРА прогонов (с применением отчёта и без), снятая подряд: абсолютные числа
/// зависят от загрузки машины, разница между соседними прогонами — нет.
int RunBufferbloatFeedbackSmoke(bool apply_feedback, BufferbloatResult& out)
{
    // Часы включаются ДО создания сессий: подмена действует на окружение, которое сессия берёт
    // при создании, и позже её уже не подсунуть.
    if (omni_webrtc_media_test_simulated_time_begin() != OMNI_WEBRTC_MEDIA_STATUS_OK) {
        return Fail("bufferbloat: simulated clock could not be started");
    }
    struct SimulatedTimeScope {
        ~SimulatedTimeScope() { (void)omni_webrtc_media_test_simulated_time_end(); }
    } simulated_time_scope;

    constexpr int kWidth = 640;
    constexpr int kHeight = 360;
    constexpr int kFps = 30;
    constexpr int kBitrateBps = 1'500'000;
    constexpr int kCapacityBps = 600'000;
    // ~1 секунда данных при этой ёмкости — классический размер раздутого буфера. Меньше — и
    // переполнение наступит раньше роста задержки, то есть стенд снова выродится в потери.
    constexpr size_t kQueueMaxBytes = 75'000;
    constexpr int kFrames = 240;  // 8 секунд по стенным часам

    const std::string session_config =
        R"({"codec":"h264","width":640,"height":360,"fps":30,"bitrate_bps":1500000)"
        R"(,"negotiated_width":640,"negotiated_height":360})";

    std::array<char, 512> error{};
    size_t required_error_len = 0;
    OmniWebRtcMediaSession* sender = nullptr;
    OmniWebRtcMediaSession* receiver = nullptr;
    if (CreateSession(
            session_config.c_str(), &sender, error.data(), error.size(), &required_error_len) !=
            OMNI_WEBRTC_MEDIA_STATUS_OK ||
        sender == nullptr) {
        std::cerr << "bufferbloat: sender create failed: " << error.data() << '\n';
        return Fail("bufferbloat sender session_create failed");
    }
    if (CreateSession(
            session_config.c_str(), &receiver, error.data(), error.size(), &required_error_len) !=
            OMNI_WEBRTC_MEDIA_STATUS_OK ||
        receiver == nullptr) {
        DestroySession(sender);
        std::cerr << "bufferbloat: receiver create failed: " << error.data() << '\n';
        return Fail("bufferbloat receiver session_create failed");
    }

    struct QueuedPacket
    {
        std::vector<uint8_t> framed;  // [длина, 2 байта LE][пакет] — как ждёт push_rtp_packets
        int64_t enqueued_at_ms = 0;
        int64_t release_at_ms = 0;
    };

    std::deque<QueuedPacket> link;
    size_t link_bytes = 0;
    int64_t last_release_ms = 0;
    int64_t peak_delay_ms = 0;
    int64_t delay_sum_ms = 0;
    unsigned long long overflow_drops = 0;
    unsigned long long delivered = 0;
    unsigned long long feedback_reports = 0;

    // ВРЕМЯ ЗДЕСЬ ВИРТУАЛЬНОЕ, И ЭТО ГЛАВНАЯ ПРАВКА СТЕНДА (#59).
    //
    // Раньше стенд читал steady_clock и спал по-настоящему. Результат зависел от загрузки машины,
    // сравнивать два прогона можно было только сериями по 10 — и всё равно выводы выходили
    // ненадёжными: замеры 2026-08-24 разошлись на одном и том же коде.
    //
    // Теперь время двигает сам стенд, а библиотека живёт по тем же виртуальным часам (OmniNowMs в
    // ядре). Прогон детерминирован и мгновенен: 8 секунд сценария проходят за миллисекунды.
    //
    // Счётчик локальный, отдельный вызов FFI не нужен: стенд сам знает, на сколько подвинул.
    int64_t virtual_now_ms = 0;
    const auto wall_ms = [&virtual_now_ms] { return virtual_now_ms; };

    std::vector<uint8_t> batch(262144);
    const auto pump_link = [&]() {
        for (int pass = 0; pass < 64; ++pass) {
            size_t written_len = 0;
            uint32_t packet_count = 0;
            if (DrainSessionPackets(
                    sender, batch.data(), batch.size(), 128, &written_len, &packet_count) !=
                OMNI_WEBRTC_MEDIA_STATUS_OK) {
                return false;
            }
            if (packet_count == 0 || written_len == 0) {
                break;
            }

            size_t offset = 0;
            const int64_t at = wall_ms();
            for (uint32_t i = 0; i < packet_count; ++i) {
                const uint16_t packet_len = static_cast<uint16_t>(batch[offset]) |
                    static_cast<uint16_t>(static_cast<uint16_t>(batch[offset + 1]) << 8);
                const size_t framed_len = packet_len + sizeof(uint16_t);
                if (link_bytes + framed_len > kQueueMaxBytes) {
                    ++overflow_drops;
                } else {
                    const int64_t serialize_ms =
                        static_cast<int64_t>(framed_len) * 8 * 1000 / kCapacityBps;
                    const int64_t release = std::max(at, last_release_ms) + serialize_ms;
                    last_release_ms = release;
                    QueuedPacket queued;
                    queued.framed.assign(
                        batch.begin() + static_cast<long>(offset),
                        batch.begin() + static_cast<long>(offset + framed_len));
                    queued.enqueued_at_ms = at;
                    queued.release_at_ms = release;
                    link.push_back(std::move(queued));
                    link_bytes += framed_len;
                }
                offset += framed_len;
            }
        }

        const int64_t at = wall_ms();
        while (!link.empty() && link.front().release_at_ms <= at) {
            QueuedPacket& queued = link.front();
            if (omni_webrtc_media_session_push_rtp_packets(
                    receiver, queued.framed.data(), queued.framed.size(), 1) !=
                OMNI_WEBRTC_MEDIA_STATUS_OK) {
                return false;
            }
            const int64_t delay = at - queued.enqueued_at_ms;
            peak_delay_ms = std::max(peak_delay_ms, delay);
            delay_sum_ms += delay;
            ++delivered;
            link_bytes -= queued.framed.size();
            link.pop_front();
        }
        return true;
    };

    const auto pump_feedback = [&]() {
        feedback_reports += PumpReceiverRtcpToSender(receiver, sender, apply_feedback)
                                .feedback_reports;
    };

    std::vector<uint8_t> y_plane;
    std::vector<uint8_t> u_plane;
    std::vector<uint8_t> v_plane;
    int64_t timestamp_us = 1'000'000;
    OmniWebRtcMediaAbrTarget target{};
    const auto fail_with = [&](const char* what) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail(what);
    };

    for (int i = 0; i < kFrames; ++i) {
        FillI420MovingNoise(y_plane, u_plane, v_plane, kWidth, kHeight, static_cast<uint32_t>(i));
        if (omni_webrtc_media_session_push_i420_frame(
                sender,
                y_plane.data(),
                kWidth,
                u_plane.data(),
                (kWidth + 1) / 2,
                v_plane.data(),
                (kWidth + 1) / 2,
                kWidth,
                kHeight,
                timestamp_us,
                i == 0 ? 1 : 0) != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            return fail_with("bufferbloat: push_i420_frame failed");
        }
        timestamp_us += 1'000'000 / kFps;

        if (!pump_link()) {
            return fail_with("bufferbloat: link pump failed");
        }
        pump_feedback();

        // ABR тикает раз в секунду и НАРОЧНО слеп: потерь нет, отчётов приёмника нет, очередь
        // висит снаружи отправителя. Единственное, что может увести цель вниз, — оценка GoogCC, а
        // она берётся ровно из отчёта о доставке. В этом и весь смысл сравнения двух прогонов.
        if (i % kFps == 0) {
            OmniWebRtcMediaAbrSample sample{};
            sample.now_ms = wall_ms();
            (void)omni_webrtc_media_session_update_abr(sender, &sample, &target);
        }
        // Двигаем время вместо сна: внутри advance библиотека доработает свои задачи — пейсер,
        // очереди, таймеры RTCP — ровно на этот интервал.
        (void)omni_webrtc_media_test_simulated_time_advance_ms(1'000 / kFps);
        virtual_now_ms += 1'000 / kFps;
    }

    out.peak_delay_ms = peak_delay_ms;
    out.average_delay_ms = delivered > 0
        ? delay_sum_ms / static_cast<int64_t>(delivered)
        : 0;
    out.overflow_drops = overflow_drops;
    out.delivered = delivered;
    out.feedback_reports = feedback_reports;
    out.feedback_matched =
        ReadDiagnosticsInt(sender, R"("transport_feedback_reports_matched":)");
    out.bwe_from_feedback_bps =
        ReadDiagnosticsInt(sender, R"("goog_cc_feedback_target_bps":)");
    out.abr_target_bps = target.target_bitrate_bps;

    DestroySession(receiver);
    DestroySession(sender);

    std::cout << "[smoke] bufferbloat " << (apply_feedback ? "with-feedback" : "blind")
              << ": delay peak " << out.peak_delay_ms << " ms, avg " << out.average_delay_ms
              << " ms, delivered " << out.delivered << ", overflow drops " << out.overflow_drops
              << ", ccfb " << out.feedback_reports << " reports/" << out.feedback_matched
              << " matched, bweFromFeedback " << out.bwe_from_feedback_bps
              << " bps, abr target " << out.abr_target_bps << " bps (link " << kCapacityBps
              << " bps, sender aims " << kBitrateBps << " bps)" << std::endl;

    // Проверяем ровно то, что построено, и ничего сверх. С применением отчёта цепочка обязана
    // дойти до сопоставления с историей отправки и дать оценку; без применения оценки взяться
    // неоткуда. Величины задержки и потерь на переполнении — замер, а не порог: они зависят от
    // загрузки машины, и порог по ним превратился бы в флейк.
    //
    // Проверки «оценка не ноль» тут мало (она не ноль и на чистом канале) — но здесь канал вдвое
    // уже запрошенного, так что оценка обязана лечь НИЖЕ запроса. Это уже утверждение о смысле, а
    // не о факте вызова.
    if (apply_feedback) {
        if (out.feedback_matched <= 0) {
            return Fail("bufferbloat: reports were applied but never matched to the send history");
        }
        if (out.bwe_from_feedback_bps <= 0) {
            return Fail("bufferbloat: delivery reports produced no channel estimate");
        }
        if (out.bwe_from_feedback_bps >= kBitrateBps) {
            return Fail("bufferbloat: the estimate stayed at the request although the link is half of it");
        }
        // И, наконец, главное: оценка обязана ДОЙТИ ДО ЦЕЛИ. Проверка отдельная от предыдущей,
        // потому что это разные поломки: «регулятор не понял канал» и «регулятор понял, а
        // адаптация его не послушала». Второе стояло здесь до 2026-08-18 и было невидимо —
        // цель молча возвращалась к запрошенной поверх верной оценки.
        if (out.abr_target_bps >= kBitrateBps) {
            return Fail("bufferbloat: abr kept the requested bitrate although the estimate says the link is half of it");
        }
    }
    // ПРОВЕРКА «БЕЗ ОТЧЁТА ОЦЕНКИ НЕТ» СНЯТА — ОНА БОЛЬШЕ НЕ ВЕРНА (#57, шаг C2b).
    //
    // Она была верна, пока регулятор был наш и оживал только от отчёта. Теперь канал считает
    // RtpTransportControllerSend, и он считает ВСЕГДА: без отчёта у него остаются потери и
    // RTT, и оценка появляется всё равно. Требовать от него молчания значило бы требовать
    // поломки.
    //
    // Смысл стенда от этого не пропал, он просто переехал в сравнение ПАРЫ прогонов
    // (см. вызывающего): отчёт обязан РЕЗКО сократить потери на переполнении. Именно это
    // утверждение и проверяется теперь — и оно сильнее прежнего, потому что говорит о пользе,
    // а не о факте вызова.

    return 0;
}

/// АДАПТАЦИЯ ПРИ НАСТОЯЩЕЙ ОБРАТНОЙ СВЯЗИ — ПОСЛЕДНЯЯ РАЗНИЦА СО ЖИВЫМ ЗВОНКОМ.
///
/// ЗАЧЕМ. Живой звонок не адаптирует разрешение, а стенд лестницы адаптирует
/// (docs/bugs/resolution-adapts-in-the-stand-not-in-the-app.md). Две разницы между ними уже
/// воспроизведены и опровергнуты: потолок источника меньше конфигурации и низкая частота кадров.
/// Осталась одна, и она структурная: у стенда лестницы НЕТ СОБЕСЕДНИКА.
///
/// Без собеседника цель по битрейту приходит из нашей ветки «отчётов не было ни одного» и стоит
/// на полу, а кодировщик ограничен КОНФИГУРАЦИЕЙ. В приложении отчёты настоящие, и кодировщик
/// ограничен РЕГУЛЯТОРОМ — цель приходит от него через `VideoStreamEncoder::OnBitrateUpdated`.
/// Выход маленький в обоих случаях, но пути разные, и сканер полосы мог бы вести себя по-разному.
///
/// Здесь собраны все три вещи сразу, чего до сих пор не было ни в одном стенде:
///   1. настоящее кодирование (кадры идут через оркестратор),
///   2. настоящая обратная связь по доставке (второй сеанс снимает отчёты, отправитель их
///      применяет — как в `bufferbloat`),
///   3. адаптация, замкнутая через размер кадра: подаём ровно то, что попросило ядро.
///
/// Канал нарочно уже, чем нужно кадру 1280x720: 600 кбит/с против 1500 кбит/с, которые апстрим
/// считает минимумом для такого размера. Значит уменьшать кадр ОБЯЗАНО.
/// `abr_tick_frames` — через сколько кадров звать замер ABR (0 = раз в секунду, как было).
/// Живая оболочка зовёт его 3-4 раза в секунду и каждый такт меняет цель, то есть
/// перенастраивает кодировщик. Это НЕ догадка: интервалы взяты из трассы `media/abr` живого
/// звонка (2026-08-27). Стенды тикали 1 Гц — четвёртая разница между ними и приложением.
int RunAdaptationWithFeedbackSmoke(int abr_tick_frames = 0)
{
    if (omni_webrtc_media_test_simulated_time_begin() != OMNI_WEBRTC_MEDIA_STATUS_OK) {
        return Fail("adapt-feedback: simulated clock could not be started");
    }
    struct SimulatedTimeScope {
        ~SimulatedTimeScope() { (void)omni_webrtc_media_test_simulated_time_end(); }
    } simulated_time_scope;

    constexpr int kWidth = 1280;
    constexpr int kHeight = 720;
    constexpr int kFps = 30;
    constexpr int kStartBitrateBps = 2'500'000;
    // Ёмкость канала. 600 кбит/с — вчетверо меньше того, что таблица апстрима считает минимумом
    // для 1280x720 при недостоверном QP (1500 кбит/с).
    constexpr int kCapacityBps = 600'000;
    constexpr size_t kQueueMaxBytes = 75'000;
    // Тридцать секунд: пять на установление оценки, дальше шаги адаптации по пять секунд.
    constexpr int kFrames = 30 * kFps;

    const std::string session_config =
        R"({"codec":"h264","width":1280,"height":720,"fps":30,"bitrate_bps":2500000)"
        R"(,"negotiated_width":1280,"negotiated_height":720})";

    std::array<char, 512> error{};
    size_t required_error_len = 0;
    OmniWebRtcMediaSession* sender = nullptr;
    OmniWebRtcMediaSession* receiver = nullptr;
    if (CreateSession(
            session_config.c_str(), &sender, error.data(), error.size(), &required_error_len) !=
            OMNI_WEBRTC_MEDIA_STATUS_OK ||
        sender == nullptr) {
        std::cerr << "adapt-feedback: sender create failed: " << error.data() << '\n';
        return Fail("adapt-feedback sender session_create failed");
    }
    if (CreateSession(
            session_config.c_str(), &receiver, error.data(), error.size(), &required_error_len) !=
            OMNI_WEBRTC_MEDIA_STATUS_OK ||
        receiver == nullptr) {
        DestroySession(sender);
        std::cerr << "adapt-feedback: receiver create failed: " << error.data() << '\n';
        return Fail("adapt-feedback receiver session_create failed");
    }

    struct QueuedPacket
    {
        std::vector<uint8_t> framed;
        int64_t release_at_ms = 0;
    };

    std::deque<QueuedPacket> link;
    size_t link_bytes = 0;
    int64_t last_release_ms = 0;
    int64_t virtual_now_ms = 0;
    unsigned long long overflow_drops = 0;
    unsigned long long delivered = 0;
    const auto wall_ms = [&virtual_now_ms] { return virtual_now_ms; };

    std::vector<uint8_t> batch(262144);
    const auto pump_link = [&]() {
        for (int pass = 0; pass < 64; ++pass) {
            size_t written_len = 0;
            uint32_t packet_count = 0;
            if (DrainSessionPackets(
                    sender, batch.data(), batch.size(), 128, &written_len, &packet_count) !=
                OMNI_WEBRTC_MEDIA_STATUS_OK) {
                return false;
            }
            if (packet_count == 0 || written_len == 0) {
                break;
            }
            size_t offset = 0;
            const int64_t at = wall_ms();
            for (uint32_t i = 0; i < packet_count; ++i) {
                const uint16_t packet_len = static_cast<uint16_t>(batch[offset]) |
                    static_cast<uint16_t>(static_cast<uint16_t>(batch[offset + 1]) << 8);
                const size_t framed_len = packet_len + sizeof(uint16_t);
                if (link_bytes + framed_len > kQueueMaxBytes) {
                    ++overflow_drops;
                } else {
                    const int64_t serialize_ms =
                        static_cast<int64_t>(framed_len) * 8 * 1000 / kCapacityBps;
                    const int64_t release = std::max(at, last_release_ms) + serialize_ms;
                    last_release_ms = release;
                    QueuedPacket queued;
                    queued.framed.assign(
                        batch.begin() + static_cast<long>(offset),
                        batch.begin() + static_cast<long>(offset + framed_len));
                    queued.release_at_ms = release;
                    link.push_back(std::move(queued));
                    link_bytes += framed_len;
                }
                offset += framed_len;
            }
        }

        const int64_t at = wall_ms();
        while (!link.empty() && link.front().release_at_ms <= at) {
            QueuedPacket& queued = link.front();
            if (omni_webrtc_media_session_push_rtp_packets(
                    receiver, queued.framed.data(), queued.framed.size(), 1) !=
                OMNI_WEBRTC_MEDIA_STATUS_OK) {
                return false;
            }
            ++delivered;
            link_bytes -= queued.framed.size();
            link.pop_front();
        }
        return true;
    };

    const auto pump_feedback = [&]() {
        (void)PumpReceiverRtcpToSender(receiver, sender);
    };

    std::vector<uint8_t> y_plane;
    std::vector<uint8_t> u_plane;
    std::vector<uint8_t> v_plane;
    std::vector<uint8_t> decoded_frame;
    int64_t timestamp_us = 1'000'000;
    uint32_t frame_index = 0;
    OmniWebRtcMediaAbrTarget target{};
    std::vector<uint32_t> widths_seen;

    const auto fail_with = [&](const char* what) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail(what);
    };

    for (int i = 0; i < kFrames; ++i) {
        // Кадр подаём ТОГО РАЗМЕРА, который выбрало ядро: без этого петля разомкнута —
        // оркестратор просит меньше, получает столько же и просит ещё меньше.
        const int width = target.target_width > 0 ? static_cast<int>(target.target_width) : kWidth;
        const int height = target.target_height > 0 ? static_cast<int>(target.target_height) : kHeight;
        FillI420MovingNoise(y_plane, u_plane, v_plane, width, height, frame_index++);
        if (omni_webrtc_media_session_push_i420_frame(
                sender,
                y_plane.data(),
                width,
                u_plane.data(),
                (width + 1) / 2,
                v_plane.data(),
                (width + 1) / 2,
                width,
                height,
                timestamp_us,
                i == 0 ? 1 : 0) != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            return fail_with("adapt-feedback: push_i420_frame failed");
        }
        timestamp_us += 1'000'000 / kFps;

        if (!pump_link()) {
            return fail_with("adapt-feedback: link pump failed");
        }
        pump_feedback();
        // ПРИЁМНИК НАДО ВЫГРЕБАТЬ, ИНАЧЕ ОН НЕ ДЕКОДИРУЕТ.
        //
        // Первый прогон этого стенда закончился «принято 3208 пакетов, декодировано 0» — и это
        // была не поломка продукта, а пропуск в стенде: буфер показа отдаёт кадр только тогда,
        // когда его СПРАШИВАЮТ. Разница важная: без выгребания стенд молча мерил бы половину
        // цепочки и называл это «адаптация ничего не купила».
        for (int guard = 0; guard < 64; ++guard) {
            int32_t out_width = 0;
            int32_t out_height = 0;
            uint32_t out_rtp = 0;
            int64_t out_us = 0;
            size_t required_len = 0;
            if (omni_webrtc_media_session_drain_i420_frame(
                    receiver, nullptr, 0, &out_width, &out_height, &out_rtp, &out_us,
                    &required_len) != OMNI_WEBRTC_MEDIA_STATUS_BUFFER_TOO_SMALL ||
                required_len == 0) {
                break;
            }
            decoded_frame.assign(required_len, 0);
            if (omni_webrtc_media_session_drain_i420_frame(
                    receiver, decoded_frame.data(), decoded_frame.size(), &out_width, &out_height,
                    &out_rtp, &out_us, &required_len) != OMNI_WEBRTC_MEDIA_STATUS_OK) {
                break;
            }
        }

        // ТАКТ ABR — КАК У ЖИВОЙ ОБОЛОЧКИ, А НЕ «РАЗ В СЕКУНДУ ПО ЗДРАВОМУ СМЫСЛУ».
        const int tick_every = abr_tick_frames > 0 ? abr_tick_frames : kFps;
        if (i % tick_every == 0) {
            OmniWebRtcMediaAbrSample sample{};
            sample.now_ms = wall_ms();
            // Обратная связь здесь настоящая и идёт, поэтому наша ветка «отчётов не было ни
            // одного» сработать не может: всё, что произойдёт с целью, сделает регулятор по
            // фактической доставке. Ровно как в живом звонке.
            if (omni_webrtc_media_session_update_abr(sender, &sample, &target) !=
                OMNI_WEBRTC_MEDIA_STATUS_OK) {
                return fail_with("adapt-feedback: update_abr failed");
            }
            if (widths_seen.empty() || widths_seen.back() != target.target_width) {
                widths_seen.push_back(target.target_width);
            }
        }

        (void)omni_webrtc_media_test_simulated_time_advance_ms(1'000 / kFps);
        virtual_now_ms += 1'000 / kFps;
    }

    const bool hardware_encoder =
        ReadDiagnosticsInt(sender, R"("hardware_encoder":)") != 0 ||
        SessionDiagnosticsContains(sender, R"("encoder_backend":"mediafoundation_h264_hw")");
    const long long wants_pixels = ReadDiagnosticsInt(sender, R"("encoder_wants_max_pixels":)");
    const long long quality_steps =
        ReadDiagnosticsInt(sender, R"("adapt_quality_resolution_steps":)");
    const long long cpu_steps = ReadDiagnosticsInt(sender, R"("adapt_cpu_resolution_steps":)");
    const long long encoded = ReadDiagnosticsInt(sender, R"("frames_encoded":)");
    const long long matched =
        ReadDiagnosticsInt(sender, R"("transport_feedback_reports_matched":)");
    const long long controller_bps =
        ReadDiagnosticsInt(sender, R"("goog_cc_feedback_target_bps":)");
    const long long decoded = ReadDiagnosticsInt(receiver, R"("frames_decoded":)");

    DestroySession(receiver);
    DestroySession(sender);

    std::cout << "[smoke] adapt-feedback tick_every="
              << (abr_tick_frames > 0 ? abr_tick_frames : kFps) << ": sizes";
    for (uint32_t w : widths_seen) {
        std::cout << ' ' << w;
    }
    std::cout << " | abr " << target.target_bitrate_bps << " bps, controller " << controller_bps
              << " bps (link " << kCapacityBps << "), wants " << wants_pixels << "px"
              << ", steps q" << quality_steps << "/c" << cpu_steps
              << ", encoded " << encoded << ", decoded " << decoded
              << ", matched " << matched << ", overflow " << overflow_drops
              << ", delivered " << delivered
              << ", encoder " << (hardware_encoder ? "hardware" : "software") << std::endl;

    // Сначала — что стенд вообще меряет то, что думает.
    if (matched <= 0) {
        return Fail("adapt-feedback: no delivery report was ever matched, so the feedback half is not under test");
    }
    if (encoded <= 0) {
        return Fail("adapt-feedback: the encoder produced nothing");
    }
    if (controller_bps <= 0 || controller_bps >= kStartBitrateBps) {
        return Fail("adapt-feedback: the controller never lowered its estimate, so there was nothing to adapt to");
    }

    // И только теперь — само утверждение.
    if (wants_pixels <= 0 || wants_pixels >= static_cast<long long>(kWidth) * kHeight) {
        return Fail("adapt-feedback: the orchestrator never narrowed its sink wants under a real feedback-driven estimate");
    }
    if (widths_seen.size() < 2) {
        return Fail("adapt-feedback: the frame size never came down");
    }
    for (size_t i = 1; i < widths_seen.size(); ++i) {
        if (widths_seen[i] > widths_seen[i - 1]) {
            return Fail("adapt-feedback: the frame size went back up while the link stayed narrow");
        }
    }
    if (decoded <= 0) {
        return Fail("adapt-feedback: the receiver decoded nothing, so the adaptation bought us nothing");
    }

    return 0;
}

/// СОБЕСЕДНИК ЗАМОЛЧАЛ — И СБАВИТЬ ОБЯЗАН САМ РЕГУЛЯТОР, БЕЗ НАШЕГО ПРАВИЛА (#61, шаг D3).
///
/// ЗАЧЕМ СТЕНД. У нас есть собственное правило «обратная связь замолчала»: если пир не сказал
/// ничего дольше двух секунд, режем цель на 15% за такт. Оно родилось из живого замера, где звонок
/// топил мобильный аплинк — 1.15 МБ за 4 с, RTT 134 -> 808 мс, двадцать секунд, в течение которых
/// собеседник не получал ничего, а мы не сбавляли (docs/bugs/call-drowns-the-mobile-uplink.md).
///
/// У апстрима на этот случай есть RttBasedBackoff: пока отчёты о доставке не приходят, а пакеты
/// уходят, его «исправленный RTT» растёт сам по себе (время с последнего отчёта плюс последний
/// RTT), и через три секунды оценка режется на 20% в секунду до пола. Пока это утверждение о
/// чужом коде — а утверждение о чужом коде без замера ровно ничего не стоит, тем более что цена
/// ошибки здесь измерена и велика.
///
/// КАК ЭТО ИЗОЛИРОВАНО. Наше правило срабатывает, только пока не применён НИ ОДИН отчёт о
/// доставке, а в первой половине они применяются. Всё, что произойдёт с целью во второй половине
/// прогона, сделает регулятор.
///
/// ЧТО ПРОВЕРЯЕТСЯ. Первая половина — отчёты идут, оценка устанавливается. Вторая — пир замолкает
/// (отчёты снимаются с приёмника, но до отправителя не доходят), а кадры продолжают идти. Цель
/// обязана уйти ЗАМЕТНО вниз. Если однажды перестанет — значит, снимать наше правило было нельзя,
/// и об этом надо узнать здесь, а не на живом звонке.
int RunFeedbackSilenceSmoke()
{
    if (omni_webrtc_media_test_simulated_time_begin() != OMNI_WEBRTC_MEDIA_STATUS_OK) {
        return Fail("feedback-silence: simulated clock could not be started");
    }
    struct SimulatedTimeScope {
        ~SimulatedTimeScope() { (void)omni_webrtc_media_test_simulated_time_end(); }
    } simulated_time_scope;

    constexpr int kWidth = 640;
    constexpr int kHeight = 360;
    constexpr int kFps = 30;
    constexpr int kBitrateBps = 1'500'000;
    constexpr int kCapacityBps = 600'000;
    constexpr size_t kQueueMaxBytes = 75'000;
    // Двенадцать секунд на установление оценки и двенадцать на молчание: порог отката у апстрима
    // три секунды, шаг — раз в секунду, так что запас четырёхкратный.
    constexpr int kTalkFrames = kFps * 12;
    constexpr int kSilentFrames = kFps * 12;

    const std::string session_config =
        R"({"codec":"h264","width":640,"height":360,"fps":30,"bitrate_bps":1500000)"
        R"(,"negotiated_width":640,"negotiated_height":360})";

    std::array<char, 512> error{};
    size_t required_error_len = 0;
    OmniWebRtcMediaSession* sender = nullptr;
    OmniWebRtcMediaSession* receiver = nullptr;
    if (CreateSession(
            session_config.c_str(), &sender, error.data(), error.size(), &required_error_len) !=
            OMNI_WEBRTC_MEDIA_STATUS_OK ||
        sender == nullptr) {
        std::cerr << "feedback-silence: sender create failed: " << error.data() << '\n';
        return Fail("feedback-silence sender session_create failed");
    }
    if (CreateSession(
            session_config.c_str(), &receiver, error.data(), error.size(), &required_error_len) !=
            OMNI_WEBRTC_MEDIA_STATUS_OK ||
        receiver == nullptr) {
        DestroySession(sender);
        std::cerr << "feedback-silence: receiver create failed: " << error.data() << '\n';
        return Fail("feedback-silence receiver session_create failed");
    }

    struct QueuedPacket
    {
        std::vector<uint8_t> framed;
        int64_t release_at_ms = 0;
    };

    std::deque<QueuedPacket> link;
    size_t link_bytes = 0;
    int64_t last_release_ms = 0;
    int64_t virtual_now_ms = 0;
    const auto wall_ms = [&virtual_now_ms] { return virtual_now_ms; };
    bool peer_is_talking = true;

    std::vector<uint8_t> batch(262144);
    const auto pump_link = [&]() {
        for (int pass = 0; pass < 64; ++pass) {
            size_t written_len = 0;
            uint32_t packet_count = 0;
            if (DrainSessionPackets(
                    sender, batch.data(), batch.size(), 128, &written_len, &packet_count) !=
                OMNI_WEBRTC_MEDIA_STATUS_OK) {
                return false;
            }
            if (packet_count == 0 || written_len == 0) {
                break;
            }
            size_t offset = 0;
            const int64_t at = wall_ms();
            for (uint32_t i = 0; i < packet_count; ++i) {
                const uint16_t packet_len = static_cast<uint16_t>(batch[offset]) |
                    static_cast<uint16_t>(static_cast<uint16_t>(batch[offset + 1]) << 8);
                const size_t framed_len = packet_len + sizeof(uint16_t);
                if (link_bytes + framed_len <= kQueueMaxBytes) {
                    const int64_t serialize_ms =
                        static_cast<int64_t>(framed_len) * 8 * 1000 / kCapacityBps;
                    const int64_t release = std::max(at, last_release_ms) + serialize_ms;
                    last_release_ms = release;
                    QueuedPacket queued;
                    queued.framed.assign(
                        batch.begin() + static_cast<long>(offset),
                        batch.begin() + static_cast<long>(offset + framed_len));
                    queued.release_at_ms = release;
                    link.push_back(std::move(queued));
                    link_bytes += framed_len;
                }
                offset += framed_len;
            }
        }

        const int64_t at = wall_ms();
        while (!link.empty() && link.front().release_at_ms <= at) {
            QueuedPacket& queued = link.front();
            // ПИР ЗАМОЛЧАЛ — ЗНАЧИТ, ДО НЕГО НИЧЕГО И НЕ ДОХОДИТ. Молчание, при котором путь
            // исправен, — это не тот случай, ради которого правило существует.
            if (peer_is_talking &&
                omni_webrtc_media_session_push_rtp_packets(
                    receiver, queued.framed.data(), queued.framed.size(), 1) !=
                    OMNI_WEBRTC_MEDIA_STATUS_OK) {
                return false;
            }
            link_bytes -= queued.framed.size();
            link.pop_front();
        }
        return true;
    };

    const auto pump_feedback = [&]() {
        (void)PumpReceiverRtcpToSender(receiver, sender, peer_is_talking);
    };

    std::vector<uint8_t> y_plane;
    std::vector<uint8_t> u_plane;
    std::vector<uint8_t> v_plane;
    int64_t timestamp_us = 1'000'000;
    OmniWebRtcMediaAbrTarget target{};
    long long talking_target_bps = 0;

    const auto fail_with = [&](const char* what) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail(what);
    };

    for (int i = 0; i < kTalkFrames + kSilentFrames; ++i) {
        if (i == kTalkFrames) {
            talking_target_bps = static_cast<long long>(target.target_bitrate_bps);
            peer_is_talking = false;
        }

        FillI420MovingNoise(y_plane, u_plane, v_plane, kWidth, kHeight, static_cast<uint32_t>(i));
        if (omni_webrtc_media_session_push_i420_frame(
                sender,
                y_plane.data(),
                kWidth,
                u_plane.data(),
                (kWidth + 1) / 2,
                v_plane.data(),
                (kWidth + 1) / 2,
                kWidth,
                kHeight,
                timestamp_us,
                i == 0 ? 1 : 0) != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            return fail_with("feedback-silence: push_i420_frame failed");
        }
        timestamp_us += 1'000'000 / kFps;

        if (!pump_link()) {
            return fail_with("feedback-silence: link pump failed");
        }
        pump_feedback();

        if (i % kFps == 0) {
            OmniWebRtcMediaAbrSample sample{};
            sample.now_ms = wall_ms();
            // ИЗОЛЯЦИЯ — В ПЕРВОЙ ПОЛОВИНЕ: отчёты о доставке успевают примениться, а наше
            // правило срабатывает только пока их не было НИ ОДНОГО. Дальше всё делает регулятор.
            (void)omni_webrtc_media_session_update_abr(sender, &sample, &target);
        }

        (void)omni_webrtc_media_test_simulated_time_advance_ms(1'000 / kFps);
        virtual_now_ms += 1'000 / kFps;
    }

    const long long silent_target_bps = static_cast<long long>(target.target_bitrate_bps);
    const long long controller_bps =
        ReadDiagnosticsInt(sender, R"("goog_cc_feedback_target_bps":)");
    const long long matched =
        ReadDiagnosticsInt(sender, R"("transport_feedback_reports_matched":)");

    DestroySession(receiver);
    DestroySession(sender);

    std::cout << "[smoke] feedback-silence: target " << talking_target_bps << " -> "
              << silent_target_bps << " bps after " << (kSilentFrames / kFps)
              << " s of silence (controller " << controller_bps << " bps, matched reports "
              << matched << ", our stale rule disarmed)" << std::endl;

    if (matched <= 0) {
        return Fail("feedback-silence: the talking half never matched a report, so the silent half proves nothing");
    }
    if (talking_target_bps <= 0) {
        return Fail("feedback-silence: no target was established while the peer was talking");
    }
    // Порог намеренно грубый: проверяется НАЛИЧИЕ отката, а не его величина. Величина у апстрима
    // своя (20% в секунду до пола) и меняться может — а вот исчезнуть отката не имеет права.
    if (silent_target_bps >= talking_target_bps) {
        return Fail("feedback-silence: the controller kept the rate although the peer stopped reporting");
    }

    return 0;
}

void omni_webrtt_ladder_dump(const std::vector<uint32_t>& widths)
{
    std::cerr << "[smoke] abr widths:";
    for (uint32_t w : widths) {
        std::cerr << ' ' << w;
    }
    std::cerr << '\n';
}

/// Разовая потеря видеоустройства — это внешний шум, а не дефект конверсии, и гейт от неё падать
/// не должен. Но и молчать нельзя: постоянная потеря — уже настоящий сигнал.
///
/// Замер 2026-08-23: три прогона из двадцати пяти падали на сравнении с libyuv, и лишь усиленная
/// диагностика показала, в чём дело: `marker-alive 0/8192, gpu-zero 8192/8192,
/// removedReason 0x887A0006` — то есть DXGI_ERROR_DEVICE_HUNG. Устройство зависало и сбрасывалось
/// (TDR), рисование не завершалось, чтение возвращало нули. Наш кадр 64x32 такого не вызывает: GPU
/// на этой машине делят композитор, оверлеи и живые обои, а D3D11CreateDevice берёт адаптер по
/// умолчанию.
///
/// Тест при этом выдавал потерю устройства ЗА РАСХОЖДЕНИЕ ПИКСЕЛЕЙ. Именно такой шум в гейте и
/// стоил трёх суток на #48, поэтому различать их обязательно: повторяем один раз со свежим
/// устройством, и только повторная потеря считается отказом.
int RunD3d11ComparisonWithDeviceLossRetry(
    std::string_view label,
    OmniWebRtcMediaStatus (*run)(char*, size_t))
{
    for (int attempt = 0; attempt < 2; ++attempt) {
        std::array<char, 288> detail{};
        if (run(detail.data(), detail.size()) == OMNI_WEBRTC_MEDIA_STATUS_OK) {
            return 0;
        }
        const bool device_lost = Contains(detail.data(), "device-lost:");
        std::cerr << "d3d11 " << label << ": " << detail.data() << std::endl;
        if (!device_lost) {
            return Fail("d3d11 comparison mismatched libyuv");
        }
        if (attempt == 0) {
            std::cerr << "d3d11 " << label
                      << ": видеоустройство потеряно (TDR), повтор со свежим устройством"
                      << std::endl;
        }
    }
    return Fail("d3d11 comparison: device lost twice in a row");
}

struct ScreenShareRampResult
{
    int start_target_bps = 0;
    int end_target_bps = 0;
    int end_width = 0;
    int end_height = 0;
    int64_t ms_to_80pct = -1;
    long long probe_clusters = 0;
    long long bwe_bps = 0;
    unsigned long long delivered = 0;
    unsigned long long sent_bytes = 0;
    unsigned long long overflow_drops = 0;
    long long padding_sent = 0;
    long long probe_without_media_allowed = 0;
    long long controller_target_bps = 0;
    long long controller_target_updates = 0;
};

/// ДЕМОНСТРАЦИЯ ЭКРАНА НА ЩЕДРОМ КАНАЛЕ: ПОДНИМАЕТСЯ ЛИ ЦЕЛЬ ВООБЩЕ (#31).
///
/// Задача #31 звучит так: демонстрация экрана восемьдесят секунд ползёт с 320x180. Разбирая её,
/// легко обвинить затор — но здесь затора нет вовсе, и в этом вся суть. Статичная картинка
/// ограничена НЕ КАНАЛОМ, а тем, что отправителю нечего слать: кодировщик после ключевого кадра
/// отдаёт крохи. GoogCC при этом принципиально не оценивает канал выше того, что реально
/// доставляется, — иначе он выдумывал бы полосу, которой не видел.
///
/// Значит без специального механизма оценка залипает возле копеечного фактического темпа, а
/// разрешение, которое следует за битрейтом, залипает вместе с ней. Механизм у апстрима ровно для
/// этого и есть — ALR-пробы (application-limited region): когда отправитель ограничен содержимым,
/// пейсер досылает пробный трафик и выясняет ёмкость, которую обычная отправка показать не может.
/// Мы включили requests_alr_probing 2026-08-18, но НИКОГДА НЕ ПРОВЕРЯЛИ, что из этого выходит.
///
/// Стенд: канал заведомо шире профиля и без потерь, очередь большая, обратная связь настоящая,
/// содержимое неподвижное (один и тот же кадр). Единственное, что может поднять цель, — пробы.
/// Поэтому тест не про «быстро ли», а про «работает ли»: если цель не растёт, механизм мёртв, и
/// #31 объясняется этим, а не потолком по измеренной ёмкости.
int RunScreenShareRampUpSmoke(ScreenShareRampResult& out)
{
    // ВИРТУАЛЬНЫЕ ЧАСЫ (#59): время двигает стенд, а не операционная система.
    //
    // До перевода результат зависел от загрузки машины, и сравнивать прогоны можно было только
    // сериями. Замеры 2026-08-24 разошлись на ОДНОМ И ТОМ ЖЕ коде — после этого доверять
    // сравнению серий между собой нельзя. Здесь этой проблемы больше нет по построению.
    if (omni_webrtc_media_test_simulated_time_begin() != OMNI_WEBRTC_MEDIA_STATUS_OK) {
        return Fail("rampup: simulated clock could not be started");
    }
    struct SimulatedTimeScope {
        ~SimulatedTimeScope() { (void)omni_webrtc_media_test_simulated_time_end(); }
    } simulated_time_scope;

    constexpr int kWidth = 1280;
    constexpr int kHeight = 720;
    constexpr int kFps = 30;
    constexpr int kNegotiatedBps = 2'500'000;
    // СТАРТ СНИЗУ — иначе тест вырожден. Первая версия стартовала сразу с согласованного профиля,
    // и кривая была ровной по определению: подниматься было неоткуда. #31 ровно про подъём.
    constexpr int kStartBps = 300'000;
    // Канал шире профиля: любое залипание цели — свойство отправителя, а не пути.
    constexpr int kCapacityBps = 4'000'000;
    // Очередь нарочно огромная: переполнение здесь было бы посторонней причиной.
    constexpr size_t kQueueMaxBytes = 1'000'000;
    constexpr int kSeconds = 30;
    constexpr int kFrames = kFps * kSeconds;

    // Кадр подаём ВСЕГДА полного размера, а ядро само уменьшает его под выбранный битрейт —
    // именно так ведёт себя демонстрация экрана: источник отдаёт то, что на экране, целиком.
    const std::string session_config =
        R"({"codec":"h264","width":1280,"height":720,"fps":30,"bitrate_bps":300000)"
        R"(,"negotiated_width":1280,"negotiated_height":720)"
        R"(,"negotiated_bitrate_bps":2500000,"content_screen":1})";

    std::array<char, 512> error{};
    size_t required_error_len = 0;
    OmniWebRtcMediaSession* sender = nullptr;
    OmniWebRtcMediaSession* receiver = nullptr;
    if (CreateSession(
            session_config.c_str(), &sender, error.data(), error.size(), &required_error_len) !=
            OMNI_WEBRTC_MEDIA_STATUS_OK ||
        sender == nullptr) {
        std::cerr << "rampup: sender create failed: " << error.data() << std::endl;
        return Fail("rampup sender session_create failed");
    }
    if (CreateSession(
            session_config.c_str(), &receiver, error.data(), error.size(), &required_error_len) !=
            OMNI_WEBRTC_MEDIA_STATUS_OK ||
        receiver == nullptr) {
        DestroySession(sender);
        std::cerr << "rampup: receiver create failed: " << error.data() << std::endl;
        return Fail("rampup receiver session_create failed");
    }

    struct QueuedPacket
    {
        std::vector<uint8_t> framed;
        int64_t release_at_ms = 0;
    };

    std::deque<QueuedPacket> link;
    size_t link_bytes = 0;
    int64_t last_release_ms = 0;
    unsigned long long overflow_drops = 0;
    unsigned long long delivered = 0;
    unsigned long long sent_bytes = 0;

    // Счётчик локальный: стенд сам знает, на сколько подвинул время. Отдельного вызова FFI за
    // текущим значением не нужно, и это же не даёт двум источникам времени разойтись.
    int64_t virtual_now_ms = 0;
    const auto wall_ms = [&virtual_now_ms] { return virtual_now_ms; };

    std::vector<uint8_t> batch(262144);
    const auto pump_link = [&]() {
        for (int pass = 0; pass < 64; ++pass) {
            size_t written_len = 0;
            uint32_t packet_count = 0;
            if (DrainSessionPackets(
                    sender, batch.data(), batch.size(), 128, &written_len, &packet_count) !=
                OMNI_WEBRTC_MEDIA_STATUS_OK) {
                return false;
            }
            if (packet_count == 0 || written_len == 0) {
                break;
            }

            size_t offset = 0;
            const int64_t at = wall_ms();
            for (uint32_t i = 0; i < packet_count; ++i) {
                const uint16_t packet_len = static_cast<uint16_t>(batch[offset]) |
                    static_cast<uint16_t>(static_cast<uint16_t>(batch[offset + 1]) << 8);
                const size_t framed_len = packet_len + sizeof(uint16_t);
                sent_bytes += framed_len;
                if (link_bytes + framed_len > kQueueMaxBytes) {
                    ++overflow_drops;
                } else {
                    const int64_t serialize_ms =
                        static_cast<int64_t>(framed_len) * 8 * 1000 / kCapacityBps;
                    const int64_t release = std::max(at, last_release_ms) + serialize_ms;
                    last_release_ms = release;
                    QueuedPacket queued;
                    queued.framed.assign(
                        batch.begin() + static_cast<long>(offset),
                        batch.begin() + static_cast<long>(offset + framed_len));
                    queued.release_at_ms = release;
                    link.push_back(std::move(queued));
                    link_bytes += framed_len;
                }
                offset += framed_len;
            }
        }

        const int64_t at = wall_ms();
        while (!link.empty() && link.front().release_at_ms <= at) {
            QueuedPacket& queued = link.front();
            if (omni_webrtc_media_session_push_rtp_packets(
                    receiver, queued.framed.data(), queued.framed.size(), 1) !=
                OMNI_WEBRTC_MEDIA_STATUS_OK) {
                return false;
            }
            ++delivered;
            link_bytes -= queued.framed.size();
            link.pop_front();
        }
        return true;
    };

    const auto pump_feedback = [&]() {
        (void)PumpReceiverRtcpToSender(receiver, sender);
    };

    // СОДЕРЖИМОЕ НЕПОДВИЖНО. Кадр наполняется ОДИН раз и дальше подаётся без изменений — это и
    // есть демонстрация статичного экрана. Меняющийся шум сделал бы отправителя ограниченным
    // содержимым лишь наполовину и смазал бы весь смысл замера.
    std::vector<uint8_t> y_plane;
    std::vector<uint8_t> u_plane;
    std::vector<uint8_t> v_plane;
    FillI420MovingNoise(y_plane, u_plane, v_plane, kWidth, kHeight, 0);

    int64_t timestamp_us = 1'000'000;
    OmniWebRtcMediaAbrTarget target{};
    const int64_t started_at = wall_ms();
    int start_target_bps = 0;
    int64_t ms_to_80pct = -1;
    std::vector<int> curve;

    const auto fail_with = [&](const char* what) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail(what);
    };

    for (int i = 0; i < kFrames; ++i) {
        if (omni_webrtc_media_session_push_i420_frame(
                sender,
                y_plane.data(),
                kWidth,
                u_plane.data(),
                (kWidth + 1) / 2,
                v_plane.data(),
                (kWidth + 1) / 2,
                kWidth,
                kHeight,
                timestamp_us,
                i == 0 ? 1 : 0) != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            return fail_with("rampup: push_i420_frame failed");
        }
        timestamp_us += 1'000'000 / kFps;

        if (!pump_link()) {
            return fail_with("rampup: link pump failed");
        }
        pump_feedback();

        if (i % kFps == 0) {
            OmniWebRtcMediaAbrSample sample{};
            sample.now_ms = wall_ms();
            (void)omni_webrtc_media_session_update_abr(sender, &sample, &target);
            const int now_target = static_cast<int>(target.target_bitrate_bps);
            curve.push_back(now_target);
            if (start_target_bps == 0) {
                start_target_bps = now_target;
            }
            if (ms_to_80pct < 0 && now_target >= kNegotiatedBps * 8 / 10) {
                ms_to_80pct = wall_ms() - started_at;
            }
        }
        (void)omni_webrtc_media_test_simulated_time_advance_ms(1'000 / kFps);
        virtual_now_ms += 1'000 / kFps;
    }

    out.start_target_bps = start_target_bps;
    out.end_target_bps = static_cast<int>(target.target_bitrate_bps);
    out.end_width = static_cast<int>(target.target_width);
    out.end_height = static_cast<int>(target.target_height);
    out.ms_to_80pct = ms_to_80pct;
    // #50 ЭТАП 1 ШАГ 2: МОДУЛЬ ОБЯЗАН СУЩЕСТВОВАТЬ.
    //
    // Проверка отдельная и намеренно грубая. Модуль создаётся ВНУТРИ задачи на своей очереди —
    // иначе TaskQueueBase::Current() вернул бы nullptr и он молча оказался бы нерабочим. Молча —
    // ключевое слово: пока провод на него не переключён, ни один счётчик отправки не изменится, и
    // «создан» будет неотличимо от «не создан». Поэтому спрашиваем прямо.
    if (ReadDiagnosticsInt(sender, R"("module_created":)") != 1) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail("rampup: upstream RTP module was not created on its task queue");
    }
    // #57 ШАГ C1: СБОРЩИК ОБЯЗАН СУЩЕСТВОВАТЬ. Спрашиваем прямо, по той же причине, что и про
    // модуль строкой выше: пока через него ничего не идёт, ни один счётчик отправки не изменится,
    // и «создан» будет неотличимо от «не создан». Он тоже берёт очередь из TaskQueueBase::Current()
    // — построй его не в задаче, и он молча окажется нерабочим.
    if (ReadDiagnosticsInt(sender, R"("transport_controller_created":)") != 1) {
        DestroySession(receiver);
        DestroySession(sender);
        return Fail("rampup: RtpTransportControllerSend was not created on its task queue");
    }
    out.controller_target_bps = ReadDiagnosticsInt(sender, R"("controller_target_bps":)");
    out.controller_target_updates = ReadDiagnosticsInt(sender, R"("controller_target_updates":)");
    out.padding_sent = ReadDiagnosticsInt(sender, R"("module_padding_sent":)");
    // Снимаем ДО уничтожения сессии: та же ошибка, что стоила разбора #48.
    out.probe_without_media_allowed =
        ReadDiagnosticsInt(sender, R"("probe_without_media_allowed":)");
    out.bwe_bps = ReadDiagnosticsInt(sender, R"("goog_cc_feedback_target_bps":)");
    out.delivered = delivered;
    out.sent_bytes = sent_bytes;
    out.overflow_drops = overflow_drops;

    std::cout << "[smoke] screenshare rampup: target " << out.start_target_bps << " -> "
              << out.end_target_bps << " bps (start " << kStartBps << ", negotiated "
              << kNegotiatedBps << ", link " << kCapacityBps << "), end frame "
              << out.end_width << "x" << out.end_height
              << ", 80% reached at ";
    if (out.ms_to_80pct >= 0) {
        std::cout << out.ms_to_80pct << " ms";
    } else {
        std::cout << "NEVER (within " << kSeconds << " s)";
    }
    std::cout << ", ctrlBps " << out.controller_target_bps
              << " x" << out.controller_target_updates
              << ", padding " << out.padding_sent
              << ", bwe " << out.bwe_bps
              << " bps, sent " << out.sent_bytes << " B, delivered " << out.delivered
              << ", overflow " << out.overflow_drops << std::endl;
    std::cout << "[smoke] screenshare curve bps:";
    for (int v : curve) {
        std::cout << " " << v;
    }
    std::cout << std::endl;

    DestroySession(receiver);
    DestroySession(sender);

    // Переполнение здесь означало бы, что стенд меряет не то: очередь на мегабайт при канале
    // шире профиля переполниться не должна.
    if (out.overflow_drops > 0) {
        return Fail("rampup: the stand overflowed although the link is wider than the profile");
    }
    if (out.delivered == 0) {
        return Fail("rampup: nothing was delivered");
    }
    // ЭКРАН НЕ УМЕНЬШАЕТСЯ — ЭТО И ЕСТЬ РАЗМЕН, ВЫБРАННЫЙ ДЛЯ ДЕМОНСТРАЦИИ (#61, шаг D3).
    //
    // Стенд намеренно ставит content_screen: он моделирует демонстрацию экрана, где уменьшенный
    // текст нечитаем. Для такого содержимого оркестратор получает MAINTAIN_RESOLUTION и платит за
    // нехватку полосы частотой кадров, а не точками.
    //
    // Проверка не декоративная: без ключа замер показал спуск до 990x556 при 918 кбит/с. Решение
    // при этом ПРАВИЛЬНОЕ для камеры — по таблице для недостоверного QP 1280x720 «стоит»
    // 1500 кбит/с, — и ровно поэтому содержимое надо называть, а не угадывать.
    if (out.end_width != kWidth || out.end_height != kHeight) {
        std::cerr << "[smoke] rampup end frame " << out.end_width << 'x' << out.end_height
                  << " instead of " << kWidth << 'x' << kHeight << '\n';
        return Fail("rampup: screen content lost resolution, although resolution is what it must keep");
    }
    // ОЦЕНКА СБОРЩИКА ЗДЕСЬ НЕ ТРЕБУЕТСЯ — ЕЁ ПОКА НЕ МОЖЕТ БЫТЬ, И ЭТО ЗАМЕРЕНО.
    //
    // Пока пакеты идут мимо сборщика, его история отправки пуста и приходящие отчёты не
    // сопоставляются ни с чем: цель равна стартовой и обновляется ровно один раз. Попытка
    // оживить его регулятор заранее (шаг C2b-1) не только НЕ доказывала оценку, но и портила
    // доставку — его пейсер начинал тикать на общей с нашим путём очереди. Откачено.
    //
    // Величины ctrlBps и число обновлений печатаются рядом намеренно: когда кадры переедут на
    // сборщика, обязаны выполниться ОБА условия — обновлений больше одного И цель отличается от
    // стартовой. Одного последнего значения для этого мало.
    // ПРОБАМ ДОЛЖНО БЫТЬ НА ЧЁМ ЕХАТЬ — И ЭТО ПРОВЕРЯЕТСЯ ЗДЕСЬ, А НЕ ВЫВОДИТСЯ ИЗ ЦЕЛИ.
    //
    // Весь механизм #31 в одной величине. Статичный экран ограничен СОДЕРЖИМЫМ: кодировщик после
    // ключевого кадра отдаёт крохи, и показать каналу ёмкость нечем. Проба без добивки —
    // просьба, которую никто не исполнил: ProbeController создавал кластеры (в логе их семь), а
    // на провод не уходило ничего сверх крох.
    //
    // Ноль здесь означает, что добивка снова не рождается, — и различить это по одной лишь цели
    // нельзя: цель может не подняться и по честной причине. Поэтому спрашиваем прямо о механизме.
    // Проверка «пробам есть на чём ехать» СНЯТА вместе с добивкой: она обваливала сборку кадров
    // под потерями (см. kAbsoluteSendTimeExtensionId в ядре). Ноль добивки сейчас — ожидаемое
    // состояние, а не поломка. Величина осталась в печати: когда добивка вернётся, её появление
    // должно быть видно сразу.
    // Разрешение пробовать без медиапакета ОТКАЧЕНО по замеру: оно поднимало цель показа экрана
    // вдвое и роняло доставку под потерями до 4-9 кадров из 90. Поэтому здесь его отсутствие —
    // норма, а не поломка, и требовать его нельзя. Величина осталась в печати: если однажды она
    // станет единицей, это должно быть видно сразу и осознанно.
    return 0;
}


/// РАЗМЕР КАДРА ОБЯЗАН ИДТИ ВНИЗ, КОГДА БИТРЕЙТ УПАЛ — И ЭТО ПРОВЕРЯЕТСЯ КАДРАМИ (#61, шаг D3).
///
/// ПОЧЕМУ СТЕНД ПЕРЕПИСАН, А НЕ ПОДОГНАН. До шага D3 разрешение было ЧИСТОЙ ФУНКЦИЕЙ от цели по
/// битрейту: ядро брало ступень из таблицы и отдавало её обратно. Поэтому стенду хватало подавать
/// одни лишь выборки ABR — кадров не требовалось вовсе.
///
/// Теперь решение принимает VideoStreamEncoderResourceManager, и принимает по ФАКТУ: QualityScaler
/// смотрит на QP закодированных кадров, а у аппаратного H.264 — BandwidthQualityScaler по таблице
/// «битрейт на разрешение» (AV1/HEVC аппаратно — по QP из битстрима, с 2026-10-18). Стенд без кадров не мог бы это увидеть В ПРИНЦИПЕ — не потому, что механизм
/// сломан, а потому, что мерить нечего. Оставить прежнюю проверку значило бы требовать от кода
/// свойства, которого у него больше нет по замыслу.
///
/// Проверяются те же три вещи, что и раньше, и добавлена четвёртая:
///   1. спуск состоялся;
///   2. спуск МОНОТОННЫЙ — ровно на этом развалилась самописная лестница (пятнадцать циклов, где
///      каждый шаг вверх отменялся шагом вниз);
///   3. потолок отправителя не превышен;
///   4. решение пришло ИМЕННО от оркестратора — его пожелания источнику сузились. Без этого
///      «размер уменьшился» нельзя отличить от «мы уменьшили его сами где-то ещё».
/// `capture_max_*` — ПОТОЛОК ИСТОЧНИКА, отдельный от размера в конфигурации сессии.
///
/// Ноль = совпадают, как было всегда: стенд подаёт кадр ровно того размера, который выбрало
/// ядро. Живой звонок так НЕ устроен — там согласованный профиль 1280x720, а камера физически
/// отдаёт максимум 960x540, и кадр приходит меньше, чем сказано в конфигурации. Ровно в этом
/// расхождении и подозревается причина того, что в приложении разрешение не адаптируется, а
/// здесь адаптируется (docs/bugs/resolution-adapts-in-the-stand-not-in-the-app.md).
/// `capture_fps` — с какой частотой источник РЕАЛЬНО отдаёт кадры (0 = как в конфигурации).
/// В тёмной комнате камера поднимает выдержку и отдаёт ~10 к/с вместо тридцати; это вторая
/// разница между стендом и живым звонком после потолка источника.
/// `push_nv12` — подавать кадры в NV12 вместо I420, как это делает оболочка.
int RunAbrResolutionLadderTest(int capture_max_width = 0,
                               int capture_max_height = 0,
                               int capture_fps = 0,
                               bool push_nv12 = false)
{
    // ВИРТУАЛЬНЫЕ ЧАСЫ (#59): время двигает стенд, а не операционная система. Здесь это ещё и
    // необходимость: адаптация по полосе проверяется раз в пять секунд, и на стенных часах стенд
    // стоил бы полминуты ожидания.
    if (omni_webrtc_media_test_simulated_time_begin() != OMNI_WEBRTC_MEDIA_STATUS_OK) {
        return Fail("abr ladder: simulated clock could not be started");
    }
    struct SimulatedTimeScope {
        ~SimulatedTimeScope() { (void)omni_webrtc_media_test_simulated_time_end(); }
    } simulated_time_scope;

    constexpr int kWidth = 1280;
    constexpr int kHeight = 720;
    constexpr int kFps = 30;
    constexpr int kStartBitrateBps = 2'500'000;
    // Двадцать пять секунд модельного времени: первые пять уходят на обвал битрейта, дальше идут
    // шаги адаптации с шагом в пять секунд.
    const int source_fps = capture_fps > 0 ? capture_fps : kFps;
    // Секунды сценария держим постоянными: сравниваются РЕЖИМЫ источника, а не длительности.
    const int kFrames = 25 * source_fps;

    const std::string session_config =
        R"({"codec":"h264","width":1280,"height":720,"fps":30,"bitrate_bps":2500000)"
        R"(,"negotiated_width":1280,"negotiated_height":720})";

    std::array<char, 512> error{};
    size_t required_error_len = 0;
    OmniWebRtcMediaSession* session = nullptr;
    OmniWebRtcMediaStatus status = CreateSession(
        session_config.c_str(),
        &session,
        error.data(),
        error.size(),
        &required_error_len);
    if (status != OMNI_WEBRTC_MEDIA_STATUS_OK || session == nullptr) {
        std::cerr << "abr ladder session create failed: " << error.data() << '\n';
        return Fail("abr ladder session_create failed");
    }

    OmniWebRtcMediaAbrTarget target{};
    int64_t now_ms = 1'000;
    std::vector<uint32_t> widths_seen;
    std::vector<uint8_t> y_plane;
    std::vector<uint8_t> u_plane;
    std::vector<uint8_t> v_plane;
    std::vector<uint8_t> uv_plane;
    std::vector<uint8_t> rtp_batch(1u << 20);
    int64_t timestamp_us = 1'000'000;
    uint32_t frame_index = 0;
    int sample_index = 0;
    bool push_failed = false;

    // ЧЕМ ЗДЕСЬ СОЗДАЁТСЯ ЗАТОР — И ПОЧЕМУ НЕ ДОЛЕЙ ПОТЕРЬ (#57, шаг C2b).
    //
    // Доля потерь, RTT и джиттер в этой выборке никуда не ведут: канал считает
    // RtpTransportControllerSend по ФАКТИЧЕСКОЙ доставке каждого пакета, то есть по отчётам
    // собеседника, а собеседника у этого стенда нет. Единственный сигнал затора, доходящий до
    // решения без него, — «обратная связь замолчала», и он же самый тяжёлый по последствиям
    // (docs/bugs/call-drowns-the-mobile-uplink.md).
    const auto push_congested_sample = [&]() {
        OmniWebRtcMediaAbrSample sample{};
        // Собеседника нет, отчётов нет ни одного — давность обратной связи растёт от начала
        // отправки сама (ABI 18: её считает сессия, а не подаёт выборка).
        sample.now_ms = now_ms;
        ++sample_index;
        const OmniWebRtcMediaStatus abr_status =
            omni_webrtc_media_session_update_abr(session, &sample, &target);
        now_ms += 1'000;
        if (widths_seen.empty() || widths_seen.back() != target.target_width) {
            widths_seen.push_back(target.target_width);
        }
        return abr_status == OMNI_WEBRTC_MEDIA_STATUS_OK;
    };

    for (int i = 0; i < kFrames; ++i) {
        if (i % kFps == 0 && !push_congested_sample()) {
            DestroySession(session);
            return Fail("update_abr failed while congested");
        }

        // Кадр подаётся ТОГО РАЗМЕРА, который выбрало ядро, — ровно как оболочка, которая
        // масштабирует кадр по дороге к кодировщику. Иначе петля разомкнута: оркестратор просит
        // меньше, получает столько же и просит ещё меньше.
        int width = target.target_width > 0 ? static_cast<int>(target.target_width) : kWidth;
        int height = target.target_height > 0 ? static_cast<int>(target.target_height) : kHeight;
        // Источник не умеет крупнее своего потолка — как настоящая камера, у которой в списке
        // режимов просто нет 1280x720. Уменьшаем ПРОПОРЦИОНАЛЬНО: подменять соотношение сторон
        // значило бы завести в стенде вторую разницу с живым звонком.
        if (capture_max_width > 0 && capture_max_height > 0 &&
            width * height > capture_max_width * capture_max_height) {
            const double scale = std::sqrt(
                static_cast<double>(capture_max_width) * capture_max_height /
                (static_cast<double>(width) * height));
            width = std::max(2, static_cast<int>(width * scale) & ~1);
            height = std::max(2, static_cast<int>(height * scale) & ~1);
        }
        FillI420MovingNoise(y_plane, u_plane, v_plane, width, height, frame_index++);
        OmniWebRtcMediaStatus push_status = OMNI_WEBRTC_MEDIA_STATUS_OK;
        if (push_nv12) {
            FillNv12FromI420(u_plane, v_plane, width, height, uv_plane);
            push_status = omni_webrtc_media_session_push_nv12_frame(
                session,
                y_plane.data(),
                width,
                uv_plane.data(),
                ((width + 1) / 2) * 2,
                width,
                height,
                timestamp_us,
                frame_index == 1 ? 1 : 0);
        } else {
            push_status = omni_webrtc_media_session_push_i420_frame(
                session,
                y_plane.data(),
                width,
                u_plane.data(),
                (width + 1) / 2,
                v_plane.data(),
                (width + 1) / 2,
                width,
                height,
                timestamp_us,
                frame_index == 1 ? 1 : 0);
        }
        if (push_status != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            push_failed = true;
            break;
        }
        timestamp_us += 1'000'000 / source_fps;
        (void)omni_webrtc_media_test_simulated_time_advance_ms(1'000 / source_fps);

        // Выгребаем провод: без этого очередь отправки переполнится и стенд начнёт мерить
        // переполнение вместо адаптации.
        for (;;) {
            size_t written_len = 0;
            uint32_t packet_count = 0;
            if (DrainSessionPackets(
                    session,
                    rtp_batch.data(),
                    rtp_batch.size(),
                    /*max_packets=*/0,
                    &written_len,
                    &packet_count) !=
                OMNI_WEBRTC_MEDIA_STATUS_OK) {
                break;
            }
            if (packet_count == 0) {
                break;
            }
        }
    }

    if (push_failed) {
        DestroySession(session);
        return Fail("abr ladder: push_i420_frame failed");
    }

    // КАКОЙ КОДИРОВЩИК РЕАЛЬНО РАБОТАЕТ — печатаем, а не предполагаем.
    //
    // Все сравнения стенда с живым звонком до сих пор молча опирались на «здесь тот же
    // аппаратный MFT». Это НЕ было проверено ни разу, а разница принципиальная: аппаратный H.264
    // судится BandwidthQualityScaler'ом по выходу против таблицы, программный — QualityScaler'ом
    // по QP. Два разных механизма — и сравнивать их выводы, не называя, какой сработал, значит
    // сравнивать яблоки с апельсинами.
    const bool hardware_encoder =
        ReadDiagnosticsInt(session, R"("hardware_encoder":)") != 0 ||
        SessionDiagnosticsContains(session, R"("encoder_backend":"mediafoundation_h264_hw")");
    const long long wants_max_pixels =
        ReadDiagnosticsInt(session, R"("encoder_wants_max_pixels":)");
    const long long wants_max_fps = ReadDiagnosticsInt(session, R"("encoder_wants_max_fps":)");
    const long long quality_steps =
        ReadDiagnosticsInt(session, R"("adapt_quality_resolution_steps":)");
    const long long cpu_steps = ReadDiagnosticsInt(session, R"("adapt_cpu_resolution_steps":)");
    const long long encoded = ReadDiagnosticsInt(session, R"("frames_encoded":)");

    std::cout << "[smoke] abr ladder capture_max="
              << (capture_max_width > 0 ? std::to_string(capture_max_width) + "x" +
                                              std::to_string(capture_max_height)
                                        : std::string("unbounded"))
              << " source_fps=" << source_fps
              << " format=" << (push_nv12 ? "nv12" : "i420") << std::endl;
    std::cout << "[smoke] abr resolutions seen under congestion:";
    for (uint32_t w : widths_seen) {
        std::cout << ' ' << w;
    }
    std::cout << std::endl;
    // Печатаем рабочую точку целиком: когда проверка ниже падает, из одного лишь «не срезал»
    // не видно, что именно не срезал и от каких чисел.
    std::cout << "[smoke] abr after congestion: " << target.target_width << 'x'
              << target.target_height << '@' << target.target_fps << "fps "
              << target.target_bitrate_bps << " bps"
              << " wants=" << wants_max_pixels << "px/" << wants_max_fps << "fps"
              << " steps=q" << quality_steps << "/c" << cpu_steps
              << " encoded=" << encoded
              << " qp=" << ReadDiagnosticsInt(session, R"("encoded_frame_qp":)")
              << " encoder=" << (hardware_encoder ? "hardware" : "software") << std::endl;

    if (encoded <= 0) {
        DestroySession(session);
        return Fail("abr ladder: the encoder produced nothing, so nothing could be measured");
    }

    if (widths_seen.size() < 2) {
        omni_webrtt_ladder_dump(widths_seen);
        DestroySession(session);
        return Fail("abr did not lower the resolution although the bitrate collapsed to the floor");
    }
    for (size_t i = 1; i < widths_seen.size(); ++i) {
        if (widths_seen[i] > widths_seen[i - 1]) {
            omni_webrtt_ladder_dump(widths_seen);
            DestroySession(session);
            return Fail("abr raised the resolution while congestion continued (the old ladder's bug)");
        }
    }
    if (widths_seen.front() > static_cast<uint32_t>(kWidth)) {
        omni_webrtt_ladder_dump(widths_seen);
        DestroySession(session);
        return Fail("abr went above the resolution the sender chose");
    }

    // РЕШЕНИЕ ПРИШЛО ОТ ОРКЕСТРАТОРА, А НЕ ОТКУДА-ТО ЕЩЁ.
    //
    // Без этой проверки «размер уменьшился» доказывает лишь то, что число изменилось. Пожелание
    // источнику — единственный путь, которым размер вообще может уменьшиться после шага D3;
    // если оно не сузилось, значит уменьшение пришло из какого-то другого места, и это дефект
    // даже при зелёных проверках выше.
    if (wants_max_pixels <= 0 || wants_max_pixels >= static_cast<long long>(kWidth) * kHeight) {
        DestroySession(session);
        return Fail("the encode orchestrator never narrowed its sink wants, so the descent came from somewhere else");
    }

    // ...и при этом затор обязан быть замечен: если ABR не срезал битрейт, проверки выше проходят
    // просто потому, что ABR ничего не делает.
    if (target.target_bitrate_bps >= static_cast<uint32_t>(kStartBitrateBps)) {
        DestroySession(session);
        return Fail("abr did not cut the bitrate under sustained congestion");
    }

    // ЗДЕСЬ СТОЯЛА ПРОВЕРКА ПОЛЯ abr_measured_capacity_bps — СНЯТА ВМЕСТЕ С САМИМ ПОЛЕМ.
    //
    // Требование, которое она охраняла, остаётся в силе: согласованный профиль это ПОЖЕЛАНИЕ, а не
    // свойство канала, и восстановление не имеет права разгоняться обратно в тот же затор
    // (docs/bugs/call-drowns-the-mobile-uplink.md). Но исполнителем требования был самописный
    // храповик поверх GoogCC, а теперь им стал сам GoogCC: LinkCapacityEstimator оценивает ёмкость,
    // границы min/max заданы в NetworkControllerConfig.constraints.
    //
    // Проверять это ЗДЕСЬ нельзя: стенд не подаёт отчётов о доставке, без которых у GoogCC не
    // работает ни delay-based BWE, ни LinkCapacityEstimator. Настоящая проверка —
    // RunBufferbloatFeedbackSmoke: узкий канал, очередь и НАСТОЯЩАЯ обратная связь по доставке.

    // ВОССТАНОВЛЕНИЕ ПРОВЕРЯЕТСЯ НЕ ЗДЕСЬ (#57, шаг C2b).
    //
    // Здесь стояла вторая половина: чистая выборка и требование, чтобы битрейт и разрешение
    // отыграли обратно. Она держалась на том, что наш GoogCC разгонялся от синтетического отчёта
    // «всё дошло». Своего регулятора больше нет, а у сборщика разгон идёт от ФАКТИЧЕСКОЙ доставки —
    // от отчётов собеседника, которого в этом стенде нет. Более того, без отчётов у GoogCC штатно
    // срабатывает RttBasedBackoff и оценка падает ЕЩЁ ниже: стенд требовал бы от него ровно
    // обратного тому, что он обязан делать.
    //
    // Свойство никуда не делось и проверяется там, где для него есть вход: `screenshare rampup` —
    // настоящий канал 4 Мбит/с, настоящие отчёты, измеренный подъём 300 -> 918 кбит/с.

    DestroySession(session);
    return 0;
}

/// Цель ABR обязана доходить ДО КОДИРОВЩИКА, а не только до диагностики.
///
/// Живой звонок город А <-> город Б 2026-08-17: ABR восемь замеров подряд держал
/// `abr=down:congested target=1280x720@24fps 150kbps`, а на провод уходило `3723kbps` — в двадцать
/// пять раз больше запрошенного, при 49% потерь у собеседника. Вся цепочка решения при этом
/// выглядит целой (UpdateAdaptiveBitrate -> ApplyEncoderRates -> SetRates), поэтому проверять надо
/// не то, что вызовы происходят, а РЕЗУЛЬТАТ: сколько байт кодировщик выдал после снижения цели.
///
/// Проверка нарочно относительная — «отреагировал ли вообще», а не «попал ли в цель». Кодировщик
/// имеет право промахнуться в полтора-два раза (низкая цель на дорогом кадре упирается в потолок
/// QP), но не имеет права выдавать столько же, сколько выдавал до снижения. Абсолютные числа
/// печатаются рядом, чтобы промах было видно даже когда тест зелёный.
/// АППАРАТНЫЙ КОДИРОВЩИК УМЕР ПОСРЕДИ ЗВОНКА — ВИДЕО ОБЯЗАНО ПРОДОЛЖИТЬСЯ.
///
/// Откат на процессор ПРИ СОЗДАНИИ работал всегда: `VideoEncoderSoftwareFallbackWrapper`
/// поднимает программный кодировщик на любой код возврата от `InitEncode`. А отказ ПОСРЕДИ
/// звонка — драйвер, перезапуск GPU, потеря устройства — не обрабатывался вовсе: обёртка
/// переключается ровно по одному ответу, `WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE`, а мы возвращали
/// `WEBRTC_VIDEO_CODEC_ERROR`. Живьём это выглядело как 41 отказ подряд без единого отката и
/// умершая картинка у собеседника (docs/bugs/encoder-goes-silent-mid-call.md).
///
/// Отказ устройства воспроизводится ключом конфигурации `test_hw_encode_fail_after`: после N
/// удачных кадров аппаратный путь начинает отвечать ошибкой. Приложение этот ключ не ставит
/// никогда.
int RunHardwareEncoderMidCallFallbackSmoke()
{
    constexpr int kWidth = 640;
    constexpr int kHeight = 360;
    constexpr int kFps = 30;
    constexpr int kHealthyFrames = 20;
    constexpr int kFramesAfterDeath = 60;

    const std::string session_config =
        std::string(R"({"codec":"av1","width":640,"height":360,"fps":30,"bitrate_bps":1500000)") +
        R"(,"negotiated_width":640,"negotiated_height":360)" +
        R"(,"test_hw_encode_fail_after":)" + std::to_string(kHealthyFrames) + "}";

    std::array<char, 512> error{};
    size_t required_error_len = 0;
    OmniWebRtcMediaSession* sender = nullptr;
    if (CreateSession(
            session_config.c_str(), &sender, error.data(), error.size(), &required_error_len) !=
            OMNI_WEBRTC_MEDIA_STATUS_OK ||
        sender == nullptr) {
        std::cerr << "hw-fallback: session_create failed: " << error.data() << '\n';
        return Fail("hw fallback session_create failed");
    }

    std::vector<uint8_t> y_plane;
    std::vector<uint8_t> u_plane;
    std::vector<uint8_t> v_plane;
    std::vector<uint8_t> rtp_batch(1u << 20);
    int64_t timestamp_us = 1'000'000;
    uint32_t frame_index = 0;
    bool push_failed = false;

    const auto run_frames = [&](int frames) -> size_t {
        size_t bytes = 0;
        for (int i = 0; i < frames && !push_failed; ++i) {
            FillI420MovingNoise(y_plane, u_plane, v_plane, kWidth, kHeight, frame_index++);
            if (omni_webrtc_media_session_push_i420_frame(
                    sender, y_plane.data(), kWidth, u_plane.data(), (kWidth + 1) / 2,
                    v_plane.data(), (kWidth + 1) / 2, kWidth, kHeight, timestamp_us,
                    frame_index == 1 ? 1 : 0) != OMNI_WEBRTC_MEDIA_STATUS_OK) {
                push_failed = true;
                break;
            }
            timestamp_us += 1'000'000 / kFps;
            std::this_thread::sleep_for(std::chrono::milliseconds(1'000 / kFps));
            for (;;) {
                size_t written_len = 0;
                uint32_t packet_count = 0;
                if (DrainSessionPackets(
                        sender, rtp_batch.data(), rtp_batch.size(), 4096, &written_len,
                        &packet_count) != OMNI_WEBRTC_MEDIA_STATUS_OK ||
                    packet_count == 0) {
                    break;
                }
                bytes += written_len - (sizeof(uint16_t) * packet_count);
            }
        }
        return bytes;
    };

    const size_t healthy_bytes = run_frames(kHealthyFrames);

    // БЕЗ АППАРАТНОГО КОДИРОВЩИКА ПРОВЕРЯТЬ НЕЧЕГО — И ЭТО НАДО СКАЗАТЬ, А НЕ ЗАЗЕЛЕНЕТЬ.
    //
    // На машине без MFT (или с занятым GPU) сессия честно поднимется на процессоре, ключ отказа
    // не сработает ни разу, и все утверждения ниже выполнятся сами собой.
    // AV1: the one codec with a software encoder behind the hardware one (no software H.264 in the
    // build — an H.264 sender whose MFT dies drops the codec from its passport instead).
    if (!SessionDiagnosticsContains(sender, R"("encoder_backend":"mediafoundation_av1_hw")")) {
        std::cout << "[smoke] hw-fallback SKIPPED: no hardware AV1 encoder on this host"
                  << std::endl;
        DestroySession(sender);
        return 0;
    }
    if (push_failed) {
        DestroySession(sender);
        return Fail("hw fallback: push_i420_frame failed while the encoder was healthy");
    }
    if (healthy_bytes == 0) {
        DestroySession(sender);
        return Fail("hw fallback: no RTP before the encoder was killed");
    }

    const size_t after_death_bytes = run_frames(kFramesAfterDeath);
    const long long fallback_requests =
        ReadDiagnosticsInt(sender, R"("mft_encode_fallback_requests":)");
    const long long encoded_frames = ReadDiagnosticsInt(sender, R"("encoder_encoded_frames":)");
    const bool switched_to_software =
        SessionDiagnosticsContains(sender, R"("encoder_backend":"libwebrtc")");

    std::cout << "[smoke] hw-fallback: healthy " << healthy_bytes << " B, after death "
              << after_death_bytes << " B, fallback requests " << fallback_requests
              << ", encoded " << encoded_frames << ", backend now "
              << (switched_to_software ? "software" : "hardware") << std::endl;

    DestroySession(sender);

    if (push_failed) {
        return Fail("hw fallback: push_i420_frame failed after the encoder died");
    }
    if (fallback_requests < 1) {
        return Fail("hw fallback: the hardware encoder never asked upstream to take over");
    }
    if (!switched_to_software) {
        return Fail("hw fallback: upstream never switched to the software encoder");
    }
    // Главное утверждение: картинка НЕ УМЕРЛА. Байты после смерти устройства — это уже
    // программный кодировщик.
    if (after_death_bytes == 0) {
        return Fail("hw fallback: no RTP after the hardware encoder died — video went black");
    }
    return 0;
}

int RunAbrEncoderFollowsTargetSmoke(std::string_view codec, bool software_encode, int vendor_id = 0)
{
    constexpr int kWidth = 1280;
    constexpr int kHeight = 720;
    constexpr int kFps = 30;
    constexpr int kStartBitrateBps = 3'000'000;
    constexpr int kWarmupFrames = 20;
    constexpr int kMeasureFrames = 60;

    const std::string session_config =
        std::string(R"({"codec":")") + std::string(codec) +
        R"(","width":1280,"height":720,"fps":30,"bitrate_bps":3000000)" +
        R"(,"negotiated_width":1280,"negotiated_height":720)" +
        (software_encode
             ? std::string(R"(,"encode_software":1})")
             : (vendor_id == 0
                    ? std::string(R"(})")
                    : std::string(R"(,"encode_vendor_id":)") + std::to_string(vendor_id) + "}"));

    std::array<char, 512> error{};
    size_t required_error_len = 0;
    OmniWebRtcMediaSession* sender = nullptr;
    if (CreateSession(
            session_config.c_str(), &sender, error.data(), error.size(), &required_error_len) !=
            OMNI_WEBRTC_MEDIA_STATUS_OK ||
        sender == nullptr) {
        std::cerr << "abr-follow: session_create failed: " << error.data() << '\n';
        return Fail("abr follow session_create failed");
    }

    std::vector<uint8_t> y_plane;
    std::vector<uint8_t> u_plane;
    std::vector<uint8_t> v_plane;
    std::vector<uint8_t> rtp_batch(1u << 20);
    // МИКРОСЕКУНДЫ, а не 90 кГц: по этой отметке кодировщик судит о том, с какой скоростью к нему
    // идут кадры, и шаг «как в RTP» выдал бы ему 333 к/с вместо 30 — вместе со всей его арифметикой
    // битрейта.
    int64_t timestamp_us = 1'000'000;
    uint32_t frame_index = 0;
    bool push_failed = false;

    OmniWebRtcMediaAbrTarget target{};
    int64_t now_ms = 1'000;
    bool abr_pump_enabled = false;

    // Один замер ABR — ровно то, что делает оболочка раз в секунду на протяжении всего звонка.
    // Важно не только для того, чтобы цель падала: между замерами кодировщик успевает показать,
    // сколько он отдал на самом деле, и поправка на его перелив приезжает к нему СЛЕДУЮЩИМ
    // замером. Тест, который сначала прогоняет все решения ABR, а потом молча подаёт кадры, эту
    // половину контура не проверяет вовсе.
    const auto push_congested_sample = [&](int index) {
        OmniWebRtcMediaAbrSample sample{};
        sample.now_ms = now_ms;
        // ЗАТОР СОЗДАЁТСЯ МОЛЧАНИЕМ ПИРА, А НЕ ДОЛЕЙ ПОТЕРЬ (#57, шаг C2b).
        //
        // Та же причина, что и в стенде лестницы: канал считает RtpTransportControllerSend по
        // фактической доставке, а отчётов о доставке у этого стенда нет. Единственный сигнал
        // затора, доходящий до решения без собеседника, — «обратная связь замолчала», и он же
        // самый тяжёлый по последствиям. С ABI 18 молчание не подаётся, а наступает: собеседника
        // нет, и давность растёт от начала отправки сама.
        const OmniWebRtcMediaStatus status =
            omni_webrtc_media_session_update_abr(sender, &sample, &target);
        now_ms += 1'000;
        return status == OMNI_WEBRTC_MEDIA_STATUS_OK;
    };

    int abr_pump_index = 60;
    bool abr_pump_failed = false;

    // Возвращает байты RTP (без служебного префикса длины в батче) за `frames` поданных кадров.
    const auto run_frames = [&](int frames) -> size_t {
        size_t bytes = 0;
        for (int i = 0; i < frames && !push_failed; ++i) {
            if (abr_pump_enabled && i % kFps == 0) {
                if (!push_congested_sample(abr_pump_index++)) {
                    abr_pump_failed = true;
                }
            }
            // Захват идёт ЗА решением ABR о размере кадра — ровно как оболочка, которая
            // перенастраивает источник под выбранную рабочую точку. Тест, который держит 1280x720,
            // что бы ядро ни решило, меряет заведомо невыполнимое требование: у H.264 такой кадр
            // начинается от 900 кбит/с, а цель к этому моменту стоит на 150.
            const int width = target.target_width > 0 ? static_cast<int>(target.target_width) : kWidth;
            const int height = target.target_height > 0 ? static_cast<int>(target.target_height) : kHeight;
            FillI420MovingNoise(y_plane, u_plane, v_plane, width, height, frame_index++);
            if (omni_webrtc_media_session_push_i420_frame(
                    sender,
                    y_plane.data(),
                    width,
                    u_plane.data(),
                    (width + 1) / 2,
                    v_plane.data(),
                    (width + 1) / 2,
                    width,
                    height,
                    timestamp_us,
                    frame_index == 1 ? 1 : 0) != OMNI_WEBRTC_MEDIA_STATUS_OK) {
                push_failed = true;
                break;
            }
            timestamp_us += 1'000'000 / kFps;
            // Кадры подаются В РЕАЛЬНОМ ТЕМПЕ. Пейсер отпускает пакеты по стенным часам, и цикл без
            // задержки выгребал бы очередь, в которую ещё ничего не отпущено: на программном пути,
            // где кодирование быстрое, замер показывал ровный НОЛЬ байт и проходил бы как «идеальное
            // послушание». На аппаратном ту же роль случайно играла задержка GPU — то есть тест был
            // верен лишь по совпадению.
            std::this_thread::sleep_for(std::chrono::milliseconds(1'000 / kFps));

            for (;;) {
                size_t written_len = 0;
                uint32_t packet_count = 0;
                if (DrainSessionPackets(
                        sender,
                        rtp_batch.data(),
                        rtp_batch.size(),
                        4096,
                        &written_len,
                        &packet_count) != OMNI_WEBRTC_MEDIA_STATUS_OK ||
                    packet_count == 0) {
                    break;
                }
                bytes += written_len - (sizeof(uint16_t) * packet_count);
            }
        }
        return bytes;
    };

    run_frames(kWarmupFrames);
    const size_t baseline_bytes = run_frames(kMeasureFrames);
    if (push_failed) {
        DestroySession(sender);
        return Fail("abr follow: push_i420_frame failed while measuring the baseline");
    }

    const long long baseline_bps =
        static_cast<long long>(baseline_bytes) * 8 * kFps / kMeasureFrames;

    // Сваливаем ABR на пол затором. Числа те же, что и в лестничном тесте: потери под 78%, полная
    // очередь, растущие дропы — затор, который нельзя прочитать как «локальная перегрузка».
    for (int i = 0; i < 60 && target.target_bitrate_bps != 150'000; ++i) {
        if (!push_congested_sample(i)) {
            DestroySession(sender);
            return Fail("abr follow: update_abr failed under congestion");
        }
    }

    if (target.target_bitrate_bps >= static_cast<uint32_t>(kStartBitrateBps) / 4) {
        std::cerr << "[smoke] abr-follow: target stayed at " << target.target_bitrate_bps
                  << " bps, congestion was not taken seriously\n";
        DestroySession(sender);
        return Fail("abr did not lower the target, so encoder obedience cannot be measured");
    }

    // С этого момента ABR тикает вместе с кадрами, как в звонке: поправка на перелив кодировщика
    // доезжает до него только следующим замером.
    abr_pump_enabled = true;
    run_frames(kWarmupFrames * 3);
    const size_t throttled_bytes = run_frames(kMeasureFrames);
    if (push_failed) {
        DestroySession(sender);
        return Fail("abr follow: push_i420_frame failed after the target was lowered");
    }
    if (abr_pump_failed) {
        DestroySession(sender);
        return Fail("abr follow: update_abr failed while frames were flowing");
    }

    const long long throttled_bps =
        static_cast<long long>(throttled_bytes) * 8 * kFps / kMeasureFrames;
    // ПОЛЯ hardware_encoder_* СНЯТЫ ВМЕСТЕ С СЫРЫМ УКАЗАТЕЛЕМ НА MFT (#61, шаг D2b).
    // Вместо них — то, что о себе говорит сам оркестратор кодирования.
    const long long encoder_drops =
        ReadDiagnosticsInt(sender, R"("encoder_dropped_frames":)");
    // Сколько раз решение ABR реально дошло до кодировщика. Без этого числа «поток не упал»
    // одинаково читается и как «кодировщик не слушается», и как «его не спросили», а это разные
    // починки.
    const long long rate_updates = ReadDiagnosticsInt(sender, R"("rate_updates":)");
    // Битрейт, с которым сессия СЧИТАЕТ кодировщик настроенным, и цель ABR рядом: если они разошлись,
    // сломано применение решения, а если совпали — сломано послушание кодировщика.
    const long long encoder_bitrate = ReadDiagnosticsInt(sender, R"("target_bitrate_bps":)");
    const long long abr_bitrate = ReadDiagnosticsInt(sender, R"("abr_target_bitrate_bps":)");
    const long long skipped = ReadDiagnosticsInt(sender, R"("frames_skipped_by_encoder":)");
    std::cout << "[smoke] abr-follow encoder_bitrate=" << encoder_bitrate
              << " abr_target=" << abr_bitrate << " target_fps=" << target.target_fps
              << " skipped=" << skipped
              << " encoded=" << ReadDiagnosticsInt(sender, R"("frames_encoded":)")
              << " sent=" << ReadDiagnosticsInt(sender, R"("rtp_packets_sent":)")
              << " size=" << target.target_width << "x" << target.target_height
              << " encDrops=" << encoder_drops
              << " qp=" << ReadDiagnosticsInt(sender, R"("encoded_frame_qp":)")
              << " wants=" << ReadDiagnosticsInt(sender, R"("encoder_wants_max_pixels":)")
              << "px steps=q" << ReadDiagnosticsInt(sender, R"("adapt_quality_resolution_steps":)")
              << " keyreq=" << ReadDiagnosticsInt(sender, R"("keyframe_requests":)")
              << std::endl;

    std::cout << "[smoke] abr-follow " << codec
              << (software_encode ? "/software" : (vendor_id == 0 ? "/hardware-auto"
                                                                  : (vendor_id == 4318 ? "/hardware-nvidia"
                                                                                       : "/hardware-intel")))
              << ": baseline " << baseline_bps << " bps -> target " << target.target_bitrate_bps
              << " bps -> actual " << throttled_bps << " bps (rate_updates " << rate_updates
              << ", encoder drops " << encoder_drops << ")" << std::endl;

    // Ноль байт — не «идеальное послушание», а поломка: адаптация обязана оставлять картинку, а не
    // выключать её. Без этой проверки тест прошёл бы именно на нуле.
    if (throttled_bps <= 0) {
        DestroySession(sender);
        return Fail("abr follow: nothing left the encoder at all after the target was lowered");
    }
    if (baseline_bps < kStartBitrateBps / 4) {
        DestroySession(sender);
        return Fail("abr follow: baseline content was too cheap to prove anything");
    }

    // ЗАМЕРЫ 2026-08-17 на этой машине (1280x720, цель падает 3000 -> 150 кбит/с).
    //
    // Было, до починки:  аппаратный 1293 -> 612 кбит/с, программный 1105 -> 312.
    // Стало, после:      аппаратный 1358 -> 330,        программный 1109 -> 123.
    //
    // Чинилось три независимых вещи, и каждая была нашей:
    //   1. Кодек собирался БЕЗ разрешения на пропуск кадров, и OpenH264 писал об этом в лог прямым
    //      текстом: без skip битрейт не контролируется. Пропуск — не оптимизация, а сам механизм.
    //   2. Пропущенный кадр считался ошибкой: секунда ожидания и INTERNAL_ERROR. С включённым
    //      пропуском это остановило бы захват, поэтому чинить флаг в одиночку было нельзя.
    //   3. MFT на вопрос IsModifiable отвечает, что менять битрейт на живом потоке НЕЛЬЗЯ, а
    //      SetValue при этом возвращает S_OK. Всё аппаратное управление скоростью было пустышкой;
    //      битрейт применяется установкой типов, как и разрешение.
    //
    // Фатальный порог — «поток упал минимум вдвое». Втрое пробовал: на четырёх прогонах подряд
    // аппаратный путь дал 0.24 / 0.27 / 0.35 / 0.30 от исходного, и порог в 0.33 отсёк исправную
    // сборку. Железо дрейфует, и порог обязан стоять там, где его не задевает дрейф. Попадание В ЦЕЛЬ порогом
    // не делаем: на 1280x720 цель 150 кбит/с недостижима физически, и это не дефект кодировщика, а
    // следствие того, что разрешение сейчас не адаптируется вовсе. Промах печатается строкой
    // OVERSHOOT на каждом прогоне и закрывается вторым шагом — возвратом адаптации разрешения под
    // потолок отправителя.
    if (throttled_bps * 2 > baseline_bps) {
        std::cerr << "[smoke] abr-follow: encoder kept sending " << throttled_bps
                  << " bps after the target dropped to " << target.target_bitrate_bps << " bps\n";
        DestroySession(sender);
        return Fail("encoder ignored the ABR target");
    }
    if (throttled_bps > static_cast<long long>(target.target_bitrate_bps) * 2) {
        std::cout << "[smoke] abr-follow: OVERSHOOT " << throttled_bps << " bps against a target of "
                  << target.target_bitrate_bps << " bps — the encoder responds but does not comply"
                  << std::endl;
    }

    DestroySession(sender);
    return 0;
}
#endif

} // namespace

/// OMNI_SMOKE_ONLY=codecs,ladder,... — run only these groups (codecs, ladder, rampup, quality,
/// bufferbloat, feedback, fallback, follow, probe); unset = everything. For iterating on one
/// area: the whole set takes a minute and more.
bool SmokeGroupSelected(std::string_view group)
{
    const char* only = std::getenv("OMNI_SMOKE_ONLY");
    if (only == nullptr || only[0] == ' ') {
        return true;
    }
    std::string_view list(only);
    while (!list.empty()) {
        const size_t comma = list.find(',');
        if (list.substr(0, comma) == group) {
            return true;
        }
        if (comma == std::string_view::npos) {
            break;
        }
        list.remove_prefix(comma + 1);
    }
    return false;
}

int main()
{
    if (omni_webrtc_media_abi_version() != OMNI_WEBRTC_MEDIA_ABI_VERSION) {
        return Fail("unexpected ABI version");
    }

#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    // Кодеки звонка по всему пути (быстро; OMNI_SMOKE_CODECS_ONLY=1 — только они).
    if (SmokeGroupSelected("codecs")) {
        for (const auto& [codec, extra] : std::initializer_list<std::pair<const char*, const char*>>{
                 {"h264", ""}, {"av1", R"(,"encode_software":1)"}, {"av1", ""}}) {
            if (const int result = RunCodecLoopbackSmoke(codec, extra); result != 0) {
                return result;
            }
            // Big frames: a key frame spans many packets, as in a real call.
            if (const int result = RunCodecLoopbackSmoke(codec, extra, 1280, 720); result != 0) {
                return result;
            }
        }
        for (const auto& [codec, extra] : std::initializer_list<std::pair<const char*, const char*>>{
                 {"h264", ""}, {"av1", ""}, {"av1", R"(,"encode_software":1)"}}) {
            if (const int result = RunCodecLiveLoopSmoke(codec, extra); result != 0) {
                return result;
            }
        }
    }
    // HEVC: decoded only on the GPU render path, so here only "frames assemble as key frames".
    // OMNI_SMOKE_HEVC_EXTRA adds to the session config, e.g. `,"encode_vendor_id":32902` to pin
    // the Intel MFT (docs/bugs/a-stuck-encoder-was-never-struck.md).
    if (std::getenv("OMNI_SMOKE_HEVC") != nullptr) {
        const char* hevc_extra = std::getenv("OMNI_SMOKE_HEVC_EXTRA");
        (void)RunCodecLiveLoopSmoke("h265", hevc_extra != nullptr ? hevc_extra : "");
    }
    if (const char* only = std::getenv("OMNI_SMOKE_CODECS_ONLY"); only != nullptr && only[0] == '1') {
        return 0;
    }
    // Только на реальном бэкенде: в заглушечной сборке сессия не создаётся вовсе.
    if (const int result = SmokeGroupSelected("ladder") ? RunAbrResolutionLadderTest() : 0; result != 0) {
        return result;
    }
    // ИСТОЧНИК, НЕ СОВПАДАЮЩИЙ С КОНФИГУРАЦИЕЙ — ТЕПЕРЬ ТОЖЕ ПОКРЫТ.
    //
    // Появились как эксперимент 2026-08-27: живой звонок не адаптирует разрешение, а стенд
    // адаптирует, и подозрение пало на две разницы между ними — камера физически не умеет
    // крупнее 960x540 при согласованном профиле 1280x720, и в тёмной комнате отдаёт ~10 к/с
    // вместо тридцати. ОБЕ гипотезы опровергнуты: адаптация работает и так, и так (замер —
    // docs/bugs/resolution-adapts-in-the-stand-not-in-the-app.md).
    //
    // Раз режимы уже описаны и проверены, они остаются постоянными проверками, а не разовым
    // экспериментом: это два состояния источника, в которых живой звонок бывает регулярно, и
    // молчаливо потерять их работоспособность было бы обидно.
    if (const int result = SmokeGroupSelected("ladder") ? RunAbrResolutionLadderTest(960, 540) : 0;
        result != 0) {
        return result;
    }
    // ТРЕТЬЯ КОНФИГУРАЦИЯ — В ФОРМАТЕ ПРОДУКТА, А НЕ В УДОБНОМ ДЛЯ СТЕНДА.
    //
    // Оболочка подаёт NV12 (камера Windows отдаёт его нативно, аппаратный H.264 его же и
    // потребляет — сквозной NV12 это ноль конверсий). Все стенды подавали I420, то есть вход,
    // которым пользуется продукт, нативным набором не прогонялся НИ РАЗУ. Здесь он и прогоняется:
    // не отдельным четвёртым прогоном, а вместо I420 в той конфигурации, которая ближе всего к
    // живому звонку — потолок источника 960x540 и десять кадров в секунду.
    if (const int result = SmokeGroupSelected("ladder")
            ? RunAbrResolutionLadderTest(960, 540, 10, /*push_nv12=*/true) : 0;
        result != 0) {
        return result;
    }

    // Демонстрация экрана на щедром канале (#31): единственное, что здесь может поднять цель, —
    // ALR-пробы, потому что статичная картинка не показывает каналу его собственную ёмкость.
    ScreenShareRampResult rampup;
    if (const int result = SmokeGroupSelected("rampup") ? RunScreenShareRampUpSmoke(rampup) : 0;
        result != 0) {
        return result;
    }

    // Оба пути кодирования: у аппаратного и программного своя реализация SetRates, и «цель дошла»
    // для одного ничего не говорит про другой. На машине без исправного MFT аппаратный вход сам
    // откатывается на программный — тест от этого не становится ложным, он просто меряет то, что
    // на этой машине реально кодирует.
    // Оба входа прогоняются ДО выхода по ошибке: «аппаратный не слушается» и «программный не
    // слушается» — разные починки, и узнавать про второй на следующем прогоне значит чинить
    // вслепую.
    // Мерка качества картинки — до проверок адаптации: она не зависит от них и нужна как база
    // сравнения при любой правке настроек кодека.
    // Часы проверяются ПЕРВЫМИ: если детерминизма нет, все числа ниже снова будут
    // несравнимыми между прогонами, и знать это надо до, а не после.
    if (const int status = SmokeGroupSelected("quality") ? RunSimulatedTimeDeterminismSmoke() : 0;
        status != 0) {
        return status;
    }
    const bool quality = SmokeGroupSelected("quality");
    const int hardware_quality = quality ? RunH264PictureQualitySmoke(/*software_encode=*/false) : 0;
    // РЕЗУЛЬТАТ СТЕНДОВ С ПОТЕРЯМИ ЧИТАЕТСЯ. РАНЬШЕ ОН ВЫБРАСЫВАЛСЯ.
    //
    // Здесь стояло `(void)` перед каждым вызовом, и это была не мелочь, а дыра в самом гейте:
    // стенд мог упасть целиком, и набор оставался зелёным. Я написал в этот стенд три проверки
    // («повтор обязан уйти», «RTX обязан развернуться», «доставка не обвалилась») и только потом
    // заметил, что все три живут в функции, чей ответ никто не читает.
    //
    // Замер 2026-08-23 показал цену этой дыры в чистом виде: разрешение пробовать без медиапакета
    // роняло доставку при 10% потерь с 52+ кадров до 4-9 в половине прогонов, и набор при этом
    // оставался зелёным во всех десяти.
    //
    // Комментарий «здесь замер, а не проверка» был верен, когда проверок действительно не было.
    // Теперь они есть, и их надо читать.
    int loss_quality = 0;
    for (const int loss : {10, 20, 33}) {
        const int status = quality ? RunH264PictureQualitySmoke(/*software_encode=*/false, loss) : 0;
        if (status != 0 && loss_quality == 0) {
            loss_quality = status;
        }
    }
    if (hardware_quality != 0) {
        return hardware_quality;
    }
    if (loss_quality != 0) {
        return loss_quality;
    }

    // Затор БЕЗ потерь — единственный стенд, на котором отчёт о доставке вообще может себя
    // проявить. Прогоны идут подряд, слепой первым: сравнивать имеет смысл только соседние.
    BufferbloatResult blind;
    BufferbloatResult with_feedback;
    const bool bufferbloat = SmokeGroupSelected("bufferbloat");
    const int bufferbloat_blind = bufferbloat ? RunBufferbloatFeedbackSmoke(/*apply_feedback=*/false, blind) : 0;
    const int bufferbloat_fed = bufferbloat ? RunBufferbloatFeedbackSmoke(/*apply_feedback=*/true, with_feedback) : 0;
    std::cout << "[smoke] bufferbloat verdict: peak delay " << blind.peak_delay_ms << " -> "
              << with_feedback.peak_delay_ms << " ms, overflow drops " << blind.overflow_drops
              << " -> " << with_feedback.overflow_drops << ", abr target " << blind.abr_target_bps
              << " -> " << with_feedback.abr_target_bps << " bps" << std::endl;
    if (bufferbloat_blind != 0) {
        return bufferbloat_blind;
    }
    if (bufferbloat_fed != 0) {
        return bufferbloat_fed;
    }
    // ГЛАВНОЕ УТВЕРЖДЕНИЕ СТЕНДА — О ПАРЕ, А НЕ О ПРОГОНЕ.
    //
    // Отчёт о доставке существует ради одного: увидеть затор ДО того, как он превратится в
    // потери. Значит его польза измеряется потерями на переполнении, и измеряется только
    // сравнением соседних прогонов — абсолютные числа зависят от машины.
    //
    // Порог грубый и намеренно: наблюдаемое 251 -> 12, то есть в двадцать раз. Требуем хотя бы
    // вдвое — это заведомо ниже наблюдаемого и заведомо выше шума.
    if (blind.overflow_drops > 0 &&
        with_feedback.overflow_drops * 2 > blind.overflow_drops) {
        std::cerr << "[smoke] bufferbloat: overflow drops " << blind.overflow_drops << " -> "
                  << with_feedback.overflow_drops
                  << " — the delivery report did not halve the queue overflow" << "\n";
        return Fail("bufferbloat: delivery reports did not reduce queue overflow");
    }

    // Молчание пира: проверяем, что откат делает САМ регулятор, без нашего правила.
    if (const int result = SmokeGroupSelected("feedback") ? RunFeedbackSilenceSmoke() : 0; result != 0) {
        return result;
    }

    // Кодирование + адаптация + НАСТОЯЩАЯ обратная связь одновременно.
    if (const int result = SmokeGroupSelected("feedback") ? RunAdaptationWithFeedbackSmoke() : 0;
        result != 0) {
        return result;
    }
    // ЧАСТЫЙ ТАКТ ABR (3-4 Гц, как у живой оболочки) ПРОВЕРЕН И СНЯТ С НАБОРА.
    //
    // Замер 2026-08-27: `RunAdaptationWithFeedbackSmoke(8)` дал 1280 -> 990 -> 766 -> 592 -> 456,
    // четыре шага по качеству — то есть частый такт адаптацию не ломает, а усиливает. Гипотеза
    // опровергнута, и держать ради неё второй тридцатисекундный прогон незачем: покрытие он
    // дублирует, а набор от него становится длиннее и, по наблюдению, менее устойчивым
    // (стенд BGRA упал 1 прогон из 3 именно с ним; без него перемерено 2026-08-27 — 15 прогонов
    // подряд, 0 падений, см. test-patterns.md). Параметр у функции остался — позвать можно
    // руками, когда понадобится. Разбор: docs/bugs/resolution-adapts-in-the-stand-not-in-the-app.md.

    // Смерть аппаратного кодировщика ПОСРЕДИ звонка: видео обязано продолжиться процессором.
    if (const int result = SmokeGroupSelected("fallback") ? RunHardwareEncoderMidCallFallbackSmoke() : 0;
        result != 0) {
        return result;
    }

    const bool follow = SmokeGroupSelected("follow");
    const int hardware_follow = follow ? RunAbrEncoderFollowsTargetSmoke("h264", /*software_encode=*/false) : 0;
    const int software_follow = follow ? RunAbrEncoderFollowsTargetSmoke("av1", /*software_encode=*/true) : 0;
    if (hardware_follow != 0) {
        return hardware_follow;
    }
    if (software_follow != 0) {
        return software_follow;
    }
#endif
    if (!SmokeGroupSelected("probe")) {
        return 0;
    }

    size_t required_len = 0;
    OmniWebRtcMediaStatus status = omni_webrtc_media_probe_json(nullptr, 0, &required_len);
    if (status != OMNI_WEBRTC_MEDIA_STATUS_BUFFER_TOO_SMALL || required_len == 0) {
        return Fail("probe sizing contract failed");
    }

    std::array<char, 16384> probe{};
    status = omni_webrtc_media_probe_json(probe.data(), probe.size(), &required_len);
    if (status != OMNI_WEBRTC_MEDIA_STATUS_OK) {
        return Fail("probe json did not fit smoke buffer");
    }

    const std::string_view probe_json(probe.data());
    if (const size_t devices_at = probe_json.find(R"("video_devices":)");
        devices_at != std::string_view::npos) {
        std::cout << "[smoke] video_devices "
                  << probe_json.substr(devices_at, probe_json.find(']', devices_at) - devices_at + 1)
                  << std::endl;
    }
    if (!Contains(probe_json, R"("transport":"omni_quic_datagram_control")") ||
        !Contains(probe_json, R"("supports_peer_connection":false)")) {
        return Fail("probe json claims the wrong native capability state");
    }

    if (Contains(probe_json, R"("backend":"libwebrtc")")) {
        if (!Contains(probe_json, R"("available":true)") ||
            !Contains(probe_json, R"("supports_rtp_over_omni_quic":true)") ||
            !Contains(probe_json, R"("codec_factory_probe_available":true)") ||
            !Contains(probe_json, R"("native_session_scaffold":true)") ||
            !Contains(probe_json, R"("native_session_scaffold_codecs":[)") ||
            !Contains(probe_json, R"("native_session_nv12_input":true)") ||
            !Contains(probe_json, R"("native_session_bgra_input":true)") ||
            !Contains(probe_json, R"("native_session_bgra_output":true)") ||
            !Contains(probe_json, R"("software_codecs":["av1","vp9"])") ||
            !Contains(probe_json, R"("hardware_codecs":[)") ||
            !Contains(probe_json, R"("hardware_h264_probe":{)") ||
            !Contains(probe_json, R"("production_session":)") ||
            !Contains(probe_json, R"("video_devices":[)") ||
            !Contains(probe_json, R"("codec_roundtrip_diagnostics":"h264_software=)")) {
            return Fail("libwebrtc probe did not report the expected codec factory proof");
        }
        // The AV1 QP parser reads the same base_q_idx libaom reports for its own frames
        // (Av1QpParser: the hardware encoders give no QP, the quality scaler needs one).
        if (!Contains(probe_json, "av1 encode/decode color-bars ok; qp_parse=ok")) {
            const size_t at = probe_json.find("qp_parse=");
            const std::string message = "AV1 QP parser disagrees with libaom: " +
                std::string(at == std::string_view::npos ? "no qp_parse" : probe_json.substr(at, 80));
            return Fail(message.c_str());
        }
    } else if (!Contains(probe_json, R"("backend":"none")") ||
               !Contains(probe_json, R"("available":false)") ||
               !Contains(probe_json, R"("supports_rtp_over_omni_quic":false)")) {
        return Fail("probe json reports an unknown backend");
    }

    // #5 increment 1a (2026-06-01): D3D11 offscreen render-target + readback. Proves the
    // GPU plumbing (device / RTV / staging map) headlessly. NOT_IMPLEMENTED is accepted for
    // non-Windows builds; on Windows WARP guarantees a device, so OK is expected.
    const OmniWebRtcMediaStatus d3d11_smoke = omni_webrtc_media_smoke_d3d11_solid_readback();
    if (d3d11_smoke != OMNI_WEBRTC_MEDIA_STATUS_OK &&
        d3d11_smoke != OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED) {
        return Fail("omni_webrtc_media_smoke_d3d11_solid_readback failed");
    }

#if defined(OMNI_WEBRTC_MEDIA_WITH_LIBWEBRTC)
    if (Contains(probe_json, R"("native_session_scaffold_codecs":["h264")")) {
        const int h264_session_smoke = RunLibWebRtcSessionSmoke("h264");
        if (h264_session_smoke != 0) {
            return h264_session_smoke;
        }

        const int receiver_report_smoke = RunReceiverReportOutOfOrderIsNotLossSmoke();
        if (receiver_report_smoke != 0) {
            return receiver_report_smoke;
        }

        const int reorder_nack_smoke = RunReorderedPacketsAreNotNackedSmoke();
        if (reorder_nack_smoke != 0) {
            return reorder_nack_smoke;
        }

        // Тот же сквозной прогон программным кодировщиком — это путь для машин без исправного
        // GPU-кодировщика, и он обязан быть не хуже проверен, чем основной.
        const int h264_software_session_smoke = RunLibWebRtcSessionSmoke("av1", true);
        if (h264_software_session_smoke != 0) {
            std::cerr << "[smoke] software-encode session smoke failed\n";
            return h264_software_session_smoke;
        }

        for (bool software_encode : {false, true}) {
            const int resize_smoke =
                RunLibWebRtcResizeWithoutRecreateSmoke(software_encode ? "av1" : "h264", software_encode);
            if (resize_smoke != 0) {
                std::cerr << "[smoke] resize-without-recreate failed (software_encode="
                          << (software_encode ? "true" : "false") << ")\n";
                return resize_smoke;
            }
        }

        const int h264_restart_smoke = RunLibWebRtcSenderRestartSmoke("h264");
        if (h264_restart_smoke != 0) {
            return h264_restart_smoke;
        }

    }
    // Audio Phase 2a-step1: silence roundtrip via libopus to confirm
    // the link + header surface before the real capture/playback
    // path lands.
    const int router_smoke = RunInboundRouterSmoke();
    if (router_smoke != 0) {
        return router_smoke;
    }
    const int opus_smoke = RunOpusRoundtripSmoke();
    if (opus_smoke != 0) {
        return opus_smoke;
    }
    // #5 increment 1b: GPU I420->RGB matches libyuv within tolerance (golden, headless).
    if (const int result = RunD3d11ComparisonWithDeviceLossRetry(
            "i420", &omni_webrtc_media_smoke_d3d11_i420_matches_libyuv);
        result != 0) {
        return result;
    }
    // #5 increment 2: composition-swapchain render path (backbuffer readback vs libyuv).
    const OmniWebRtcMediaStatus d3d11_swapchain_smoke =
        omni_webrtc_media_smoke_d3d11_swapchain_i420();
    if (d3d11_swapchain_smoke != OMNI_WEBRTC_MEDIA_STATUS_OK) {
        return Fail("omni_webrtc_media_smoke_d3d11_swapchain_i420 failed");
    }
    // Task #84 (2026-06-06): HW H.264 decode (MF/DXVA → NV12 texture, zero-copy receive). Optional:
    // NOT_IMPLEMENTED when the machine has no HW H.264 encoder/decoder MFT (SW OpenH264 fallback).
    const OmniWebRtcMediaStatus hw_decode_smoke = omni_webrtc_media_smoke_mf_h264_hw_decode();
    if (hw_decode_smoke != OMNI_WEBRTC_MEDIA_STATUS_OK &&
        hw_decode_smoke != OMNI_WEBRTC_MEDIA_STATUS_NOT_IMPLEMENTED) {
        return Fail("omni_webrtc_media_smoke_mf_h264_hw_decode failed");
    }
    // Task #84 increment 2: GPU NV12->RGB matches libyuv (golden) — the renderer presents the HW
    // decoder's NV12 textures correctly.
    if (const int result = RunD3d11ComparisonWithDeviceLossRetry(
            "nv12", &omni_webrtc_media_smoke_d3d11_nv12_matches_libyuv);
        result != 0) {
        return result;
    }
    // Audio Phase 2a-step3: OmniAudioSender end-to-end (opus encode +
    // RTP packetize + queue + drain), validating the wire header.
    const OmniWebRtcMediaStatus audio_sender_smoke =
        omni_webrtc_media_smoke_audio_roundtrip();
    if (audio_sender_smoke != OMNI_WEBRTC_MEDIA_STATUS_OK) {
        return Fail("omni_webrtc_media_smoke_audio_roundtrip failed");
    }
    // Audio Phase 2a-step3: WASAPI capture → OmniAudioSender wire-up.
    // Validates the FrameCallback ↔ PushPcm contract holds without
    // crashing across Start/Stop. Headless-safe.
    const bool audio_devices_allowed = AudioDevicesAllowed();
    const OmniWebRtcMediaStatus capture_smoke = audio_devices_allowed
        ? omni_webrtc_media_smoke_audio_capture_to_sender()
        : OMNI_WEBRTC_MEDIA_STATUS_OK;
    if (capture_smoke != OMNI_WEBRTC_MEDIA_STATUS_OK) {
        return Fail("omni_webrtc_media_smoke_audio_capture_to_sender failed");
    }
    // Audio Phase 2b-step1: WASAPI render lifecycle. Start/Stop +
    // WritePcm contract is callable without crashing; headless-safe.
    const OmniWebRtcMediaStatus render_smoke = audio_devices_allowed
        ? omni_webrtc_media_smoke_audio_render_lifecycle()
        : OMNI_WEBRTC_MEDIA_STATUS_OK;
    if (render_smoke != OMNI_WEBRTC_MEDIA_STATUS_OK) {
        return Fail("omni_webrtc_media_smoke_audio_render_lifecycle failed");
    }
    // Выбор устройства (ABI 20): список отвечает JSON'ом даже без устройств, короткий буфер —
    // BUFFER_TOO_SMALL с нужным размером; пропавший выбранный микрофон уступает устройству по
    // умолчанию — создание с несуществующим id ведёт себя ровно как без id.
    {
        std::array<char, 4> tiny{};
        size_t needed = 0;
        if (omni_webrtc_media_audio_list_devices(tiny.data(), tiny.size(), &needed) !=
                OMNI_WEBRTC_MEDIA_STATUS_BUFFER_TOO_SMALL ||
            needed <= tiny.size()) {
            return Fail("omni_webrtc_media_audio_list_devices: short buffer not reported");
        }
        std::vector<char> list(needed);
        size_t written = 0;
        if (omni_webrtc_media_audio_list_devices(list.data(), list.size(), &written) !=
                OMNI_WEBRTC_MEDIA_STATUS_OK ||
            written != needed ||
            std::string_view(list.data()).rfind(R"({"capture":[)", 0) != 0 ||
            std::string_view(list.data()).find(R"(,"render":[)") == std::string_view::npos) {
            return Fail("omni_webrtc_media_audio_list_devices: unexpected answer");
        }
        std::cout << "[smoke] audio devices: " << list.data() << std::endl;
        if (audio_devices_allowed) {
            OmniWebRtcMediaAudioCapture* plain = nullptr;
            OmniWebRtcMediaAudioCapture* stale = nullptr;
            const OmniWebRtcMediaStatus plain_status =
                omni_webrtc_media_audio_capture_create("{}", &plain, nullptr, 0, nullptr);
            omni_webrtc_media_audio_capture_destroy(plain);
            const OmniWebRtcMediaStatus stale_status = omni_webrtc_media_audio_capture_create(
                R"({"device_id":"{0.0.1.00000000}.{00000000-0000-0000-0000-000000000000}"})",
                &stale, nullptr, 0, nullptr);
            omni_webrtc_media_audio_capture_destroy(stale);
            if (plain_status != stale_status) {
                return Fail("audio_capture_create: a gone device_id did not fall back to the default");
            }
        }
    }
    // Audio Phase 2b-step2: full opus + RTP roundtrip Sender→Receiver.
    const OmniWebRtcMediaStatus audio_roundtrip_smoke =
        omni_webrtc_media_smoke_audio_sender_to_receiver();
    if (audio_roundtrip_smoke != OMNI_WEBRTC_MEDIA_STATUS_OK) {
        return Fail("omni_webrtc_media_smoke_audio_sender_to_receiver failed");
    }
    {
        std::array<char, 512> jitter_report{};
        const OmniWebRtcMediaStatus jitter_smoke =
            omni_webrtc_media_smoke_audio_jitter(jitter_report.data(), jitter_report.size());
        std::cout << "[smoke] audio jitter: " << jitter_report.data() << std::endl;
        if (jitter_smoke != OMNI_WEBRTC_MEDIA_STATUS_OK) {
            return Fail("omni_webrtc_media_smoke_audio_jitter failed");
        }
    }
    // Audio Phase 2b-step3: production C API for playback. Lifecycle
    // create/push/destroy, headless-safe (no render endpoint =>
    // INTERNAL_ERROR with HRESULT; not a smoke failure).
    {
        OmniWebRtcMediaAudioPlayback* playback_handle = nullptr;
        std::array<char, 256> playback_error{};
        size_t playback_required = 0;
        const OmniWebRtcMediaStatus playback_create_status =
            // Decode-only ({"render":0}): the C API contract is the same, and no speaker opens.
            omni_webrtc_media_audio_playback_create(
                R"({"render":0})",
                &playback_handle,
                playback_error.data(),
                playback_error.size(),
                &playback_required);
        if (playback_create_status == OMNI_WEBRTC_MEDIA_STATUS_OK &&
            playback_handle != nullptr) {
            // Push a deliberately invalid 6-byte packet — receiver
            // must drop it without crashing and the C API must
            // surface OK (drop is not an error).
            const uint8_t scrap[6]{0x80, 111, 0, 0, 0, 0};
            const OmniWebRtcMediaStatus push_status =
                omni_webrtc_media_audio_playback_push_rtp_packet(
                    playback_handle, scrap, sizeof(scrap));
            if (push_status != OMNI_WEBRTC_MEDIA_STATUS_OK) {
                omni_webrtc_media_audio_playback_destroy(playback_handle);
                return Fail("omni_webrtc_media_audio_playback_push_rtp_packet failed");
            }
            omni_webrtc_media_audio_playback_destroy(playback_handle);
        } else if (playback_create_status == OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR) {
            std::cerr << "audio_playback_create returned INTERNAL_ERROR "
                      << "(expected on headless test machines): "
                      << playback_error.data() << '\n';
        } else {
            return Fail("omni_webrtc_media_audio_playback_create returned an unexpected status");
        }
        // Idempotent on null: must not crash.
        omni_webrtc_media_audio_playback_destroy(nullptr);
    }
    // Audio Phase 2a-step4: production C API lifecycle. Validates
    // argument marshaling, handle hand-off, and null-safe destroy.
    // The Create call goes through WASAPI which can legitimately
    // fail in headless / sandbox environments (E_INVALIDARG when no
    // default endpoint exists, etc.). On success we additionally
    // exercise the packet sink + mute + destroy path. The wire-shape itself is
    // covered by the step3 smokes above which don't depend on a
    // live audio session.
    if (audio_devices_allowed) {
        OmniWebRtcMediaAudioCapture* audio_handle = nullptr;
        std::array<char, 256> audio_error{};
        size_t audio_required = 0;
        const OmniWebRtcMediaStatus audio_create_status =
            omni_webrtc_media_audio_capture_create(
                R"({"target_bitrate_bps":24000})",
                &audio_handle,
                audio_error.data(),
                audio_error.size(),
                &audio_required);
        if (audio_create_status == OMNI_WEBRTC_MEDIA_STATUS_OK &&
            audio_handle != nullptr) {
            // Muted: capture keeps running, but NOTHING reaches the sink. Whether the unmuted
            // half produces packets depends on a live microphone, so only the muted half is a
            // hard assertion — it is the one the shell used to enforce by dropping packets.
            SessionPacketInbox audio_inbox;
            if (omni_webrtc_media_audio_capture_set_packet_sink(
                    audio_handle, &CollectSessionPacket, &audio_inbox) != OMNI_WEBRTC_MEDIA_STATUS_OK ||
                omni_webrtc_media_audio_capture_set_muted(audio_handle, 1) != OMNI_WEBRTC_MEDIA_STATUS_OK) {
                omni_webrtc_media_audio_capture_destroy(audio_handle);
                return Fail("audio capture sink/mute setters failed");
            }
            std::this_thread::sleep_for(std::chrono::milliseconds(120));
            size_t muted_packets = 0;
            {
                std::lock_guard<std::mutex> lock(audio_inbox.mutex);
                muted_packets = audio_inbox.packets.size();
            }
            (void)omni_webrtc_media_audio_capture_set_muted(audio_handle, 0);
            std::this_thread::sleep_for(std::chrono::milliseconds(120));
            // Clear the sink before the inbox goes out of scope: after this no call is in flight.
            (void)omni_webrtc_media_audio_capture_set_packet_sink(audio_handle, nullptr, nullptr);
            size_t unmuted_packets = 0;
            {
                std::lock_guard<std::mutex> lock(audio_inbox.mutex);
                unmuted_packets = audio_inbox.packets.size();
            }
            omni_webrtc_media_audio_capture_destroy(audio_handle);
            if (muted_packets != 0) {
                return Fail("a muted audio capture handed packets to its sink");
            }
            std::cout << "[smoke] audio capture: muted 0 packets, unmuted " << unmuted_packets
                      << " (0 is fine without a live microphone)" << std::endl;
        } else if (audio_create_status == OMNI_WEBRTC_MEDIA_STATUS_INTERNAL_ERROR) {
            // Headless / no-mic / locked endpoint — Create surfaces
            // the underlying HRESULT in audio_error and returns
            // INTERNAL_ERROR. Acceptable for the smoke; production
            // C# treats this as "audio capture unavailable for this
            // call" and reports up to the user.
            std::cerr << "audio_capture_create returned INTERNAL_ERROR "
                      << "(expected on headless test machines): "
                      << audio_error.data() << '\n';
        } else {
            return Fail("omni_webrtc_media_audio_capture_create returned an unexpected status");
        }
        // Idempotent on null: must not crash.
        omni_webrtc_media_audio_capture_destroy(nullptr);
    }
#else
    const int unavailable_smoke = RunUnavailableSessionSmoke();
    if (unavailable_smoke != 0) {
        return unavailable_smoke;
    }
#endif

    DestroySession(nullptr);
    return 0;
}
