using System.Buffers;
using System.Buffers.Binary;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.WindowsRuntime;
using System.Security.Cryptography;
using System.Text.Json;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media.Imaging;
using Omni.Windows.App.Core;
using Windows.Graphics.Imaging;
using Windows.Foundation;
using Windows.Media;
using Windows.Media.Capture;
using Windows.Media.Capture.Frames;
using Windows.Media.Devices;
using Windows.Media.MediaProperties;
using WinRT;

namespace Omni.Windows.App.Features.Calls;

// The ABR tick. This used to be the media control plane: a 50 ms loop that drained NACK / PLI /
// receiver and sender reports out of the native sessions, carried them over the core's control
// channel and the measured RTT from one half of the call to the other. All of that is real RTCP
// now (ABI 16–19): it leaves the native modules with the media and reaches the peer's native
// router with no shell tick on the way — docs/bugs/media-rode-the-shells-timer.md, steps 3–5.
// What remains is a clock: the native session decides, the shell applies the decision to capture
// (frame size) and records it.
internal sealed partial class CallVideoService
{
    /// Как часто спрашивать у нативной сессии решение ABR. Обратная связь через этот такт больше
    /// не ходит, поэтому и ручки (OMNI_MEDIA_CONTROL_LOOP_MS) больше нет: такт ничего не задерживает.
    private const int AbrTickMs = 50;

    private async Task AbrLoopAsync(CancellationToken token)
    {
        while (!token.IsCancellationRequested)
        {
            try
            {
                if (rtcpFeedbackEnabled)
                {
                    ReadMeasuredPathRtt();
                    ApplyAdaptiveBitrateOnTick();
                }
                await Task.Delay(AbrTickMs, token);
            }
            catch (OperationCanceledException)
            {
                throw;
            }
            catch (Exception ex)
            {
                RecordError($"Media ABR tick failed: {FormatExceptionForDiagnostics(ex)}");
                await Task.Delay(100, token);
            }
        }
    }

    /// Проход туда-обратно, измеренный отправляющей половиной по отчётам о доставке, — для
    /// диагностики (`pathRtt`). Приёмной половине его не переносим: её модуль меряет свой по XR.
    /// Читаем всегда, когда есть кому мерить: у собеседника без камеры приёмной половины нет.
    private void ReadMeasuredPathRtt()
    {
        NativeWebRtcMediaEngine.Session? sender = nativeSenderSession;
        if (sender is null)
        {
            return;
        }

        lock (sender.Gate)
        {
            long rttUs = sender.TransportRttUs();
            if (rttUs >= 0)
            {
                Interlocked.Exchange(ref diagnosticsMeasuredPathRttUs, rttUs);
            }
        }
    }

    /// <summary>
    /// ABR on a clock, not only when feedback arrives. Silence is exactly when holding the target
    /// is wrong: our own bytes keep a drowned path full (docs/bugs/call-drowns-the-mobile-uplink.md).
    /// The peer's loss/jitter/RTT and how long it has been silent are the native session's own
    /// knowledge since ABI 18 — the RR reaches its sender module as real RTCP.
    /// </summary>
    private void ApplyAdaptiveBitrateOnTick()
    {
        if (!localCameraActive && !localScreenShareRequested)
        {
            // Nothing is being sent — there is no bitrate to give back.
            return;
        }

        RunAdaptiveBitrate();
    }

    /// Свой ключевой кадр (шаг ABR вниз, смена источника). Просьбы собеседника сюда не идут —
    /// их PLI принимает нативный отправитель сам (EncoderRtcpFeedback).
    private void ForceKeyFrame()
    {
        NativeWebRtcMediaEngine.Session? sender = nativeSenderSession;
        if (sender is null)
        {
            return;
        }

        lock (sender.Gate)
        {
            sender.RequestKeyFrame();
        }
    }

    private void RunAdaptiveBitrate()
    {
        NativeWebRtcMediaAbrSample sample = new(Environment.TickCount64);
        NativeWebRtcMediaAbrTarget target;
        NativeWebRtcMediaEngine.Session? sender = nativeSenderSession;
        if (sender is null)
        {
            return;
        }

        lock (sender.Gate)
        {
            target = sender.UpdateAdaptiveBitrate(sample);
        }

        adaptiveDiagnostics = target.StateText;
        if (!target.Changed)
        {
            return;
        }

        // Every rate change, with the inputs that produced it. ABR deciding to halve the frame
        // rate is the difference between a smooth call and a choppy one, and until now it left no
        // record: the reason string lives in a diagnostics line nobody is sampling at that exact
        // moment. A stutter reported after the fact was unanswerable — see docs/bugs/media-hitching.md.
        Core.AppTrace.Write(
            "media/abr",
            $"{targetFps}fps/{targetBitrateBps / 1000}kbps -> {target.Fps}fps/{target.BitrateBps / 1000}kbps " +
            $"{target.Width}x{target.Height} state={target.StateText} downshift={target.IsDownshift} " +
            $"pathRtt={FormatMeasuredPathRtt()} " +
            $"renderStalls={Interlocked.Read(ref renderStalls)} " +
            $"enc={CurrentAverageEncodeMs()}ms cap={CurrentAverageCaptureProcessingMs()}ms " +
            $"sentFps={Interlocked.Read(ref diagnosticsCurrentSentFps)}");

        adaptiveBitrateBps = target.BitrateBps;
        adaptiveFps = target.Fps;
        targetBitrateBps = target.BitrateBps;
        lastSenderAbrBitrateBps = target.BitrateBps;
        targetFps = target.Fps;
        targetFrameIntervalMs = Math.Max(1, 1000 / target.Fps);
        if (target.IsDownshift)
        {
            ForceKeyFrame();
        }

        // РАЗРЕШЕНИЕ ПРИМЕНЯЕТСЯ МАСШТАБИРОВАНИЕМ, А НЕ ПЕРЕЗАПУСКОМ ЗАХВАТА.
        //
        // Здесь успели постоять два неверных варианта. Сначала (Phase B, 2026-05-18) — перезапуск
        // захвата под размер, выбранный ядром: правильное решение, применённое самым дорогим
        // способом, потому что переоткрытие камеры стоит сотен миллисекунд чёрного кадра. Потом
        // (2026-08-17, утро) — не применять размер вовсе: это убрало запинку вместе с самой
        // адаптацией, и в тот же вечер выяснилось, чего это стоит — мы просили у 1280x720 те
        // 150 кбит/с, от которых у H.264 начинается 320x180.
        //
        // Правильный способ — тот же, что у VideoAdapter в WebRTC: источник продолжает идти в
        // своём режиме, а кадр масштабируется по дороге к кодировщику (CreateAdaptiveEncodeFrame).
        // Смена размера стоит одного ключевого кадра и ни одного потерянного кадра захвата.
        //
        // Выбор отправителя при этом остаётся ПОТОЛКОМ: ядро никогда не отдаёт размер больше
        // согласованного, а перенастройку самого захвата по-прежнему зовёт только выбор человека.
        if (target.Width > 0 && target.Height > 0)
        {
            targetCaptureWidth = Math.Min(negotiatedCaptureWidth, (int)target.Width);
            targetCaptureHeight = Math.Min(negotiatedCaptureHeight, (int)target.Height);
        }
    }
}
