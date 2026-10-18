using System.Buffers.Binary;
using System.Runtime.InteropServices;
using System.Text.Json;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Calls;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class RealtimeMediaContractTests
{
    [Fact]
    public void CodecCapabilities_ComeFromNativeProbeWithStaticPayloadTypes()
    {
        IReadOnlyList<CallVideoCodecSupport> codecs = CallVideoCodecFactory.GetSupportedCodecs();
        NativeWebRtcMediaProbeResult probe = CallVideoCodecFactory.GetNativeWebRtcProbeResult();

        if (!probe.NativeSessionScaffold)
        {
            Assert.Empty(codecs);
            return;
        }

        Assert.DoesNotContain(codecs, codec => codec.Kind == CallVideoCodecKind.H264 && codec.EncodeHardware == false);
        Assert.All(codecs, codec => Assert.InRange(codec.PayloadType, 1, 127));
    }

    // Each direction its own codec (docs/decisions/per-pair-video-codec-selection.md): the media
    // service runs the selection with THIS side's two directions attached from the snapshot.
    [Fact]
    public void NegotiationSnapshot_AttachesThisSidesDirectionsToTheSelection()
    {
        const string json = """
            {"dialog_key":"d","status":"negotiated","updated_at_unix_ms":1,
             "selected":{"dialog_key":"d","session_id":"s","max_datagram_bytes":1000,"selected_at_unix_ms":1,
               "video":{"streams":[
                 {"sender_capabilities_id":"me","codec":"av1","payload_type":45,"width":1920,"height":1080,"fps":30,"target_bitrate_bps":2400000,"reason":"x"},
                 {"sender_capabilities_id":"peer","codec":"h265","payload_type":49,"width":1280,"height":720,"fps":30,"target_bitrate_bps":2800000,"reason":"y"}],
                 "capture_pixel_format":"nv12","rtcp_feedback_enabled":true}},
             "send_video":{"sender_capabilities_id":"me","codec":"av1","payload_type":45,"width":1920,"height":1080,"fps":30,"target_bitrate_bps":2400000,"reason":"x"},
             "receive_video":{"sender_capabilities_id":"peer","codec":"h265","payload_type":49,"width":1280,"height":720,"fps":30,"target_bitrate_bps":2800000,"reason":"y"}}
            """;
        RealtimeMediaNegotiationSnapshot snapshot = JsonSerializer.Deserialize<RealtimeMediaNegotiationSnapshot>(json)!;
        RealtimeMediaSelection selection = snapshot.LocalSelection!;
        Assert.Equal("av1", selection.SendStream!.Codec);
        Assert.Equal("h265", selection.ReceiveStream!.Codec);
        Assert.Equal(CallVideoCodecKind.Av1, CallVideoCodecFactory.ParseCodec(selection.SendStream.Codec));
        Assert.Equal(
            "send=av1 1920x1080@30 2400kbps recv=h265 1280x720@30 2800kbps",
            CallVideoService.FormatDirections(selection.SendStream, selection.ReceiveStream));
    }

    // «Декодирование/Кодирование: Программно» in Settings: the passport stops promising what only the
    // video card does (H.264 and HEVC have no software path at all).
    [Fact]
    public void Passport_FollowsTheSoftwareOnlySettings()
    {
        IReadOnlyList<CallVideoCodecSupport> all =
        [
            new CallVideoCodecSupport(CallVideoCodecKind.Av1, CallVideoRtpConstants.Av1PayloadType, true, false, "av1"),
            new CallVideoCodecSupport(CallVideoCodecKind.H265, CallVideoRtpConstants.H265PayloadType, true, true, "h265"),
            new CallVideoCodecSupport(CallVideoCodecKind.H264, CallVideoRtpConstants.H264PayloadType, true, true, "h264"),
        ];
        Assert.Same(all, CallVideoCodecFactory.RestrictToDevices(all, false, false));

        IReadOnlyList<CallVideoCodecSupport> decodeOnCpu = CallVideoCodecFactory.RestrictToDevices(all, false, true);
        Assert.Null(decodeOnCpu.Single(c => c.Kind == CallVideoCodecKind.H264).DecodeHardware);
        Assert.Equal(true, decodeOnCpu.Single(c => c.Kind == CallVideoCodecKind.H264).EncodeHardware);
        Assert.Null(decodeOnCpu.Single(c => c.Kind == CallVideoCodecKind.H265).DecodeHardware);
        Assert.Equal(true, decodeOnCpu.Single(c => c.Kind == CallVideoCodecKind.H265).EncodeHardware);

        IReadOnlyList<CallVideoCodecSupport> bothOnCpu = CallVideoCodecFactory.RestrictToDevices(all, true, true);
        Assert.DoesNotContain(bothOnCpu, c => c.Kind == CallVideoCodecKind.H265);
        Assert.DoesNotContain(bothOnCpu, c => c.Kind == CallVideoCodecKind.H264);
        Assert.Equal(false, bothOnCpu.Single(c => c.Kind == CallVideoCodecKind.Av1).EncodeHardware);
    }

    [Fact]
    public void MediaDiagnostics_SayEachDirectionsCodecInWords()
    {
        string text = CallVideoService.FormatVideoDirectionsForPeople(
            new RealtimeVideoStream("me", "av1", 45, 1920, 1080, 30, 2_400_000, "лучший из возможных"),
            new RealtimeVideoStream("peer", "h265", 49, 1280, 720, 30, 2_800_000, "отправитель не кодирует AV1"));
        Assert.Equal(
            "Ваше видео: AV1, 1920×1080, 30 к/с — лучший из возможных\nВидео собеседника: HEVC, 1280×720, 30 к/с — отправитель не кодирует AV1\n",
            text);
        Assert.Equal(string.Empty, CallVideoService.FormatVideoDirectionsForPeople(null, null));
        Assert.StartsWith("Ваше видео: не идёт", CallVideoService.FormatVideoDirectionsForPeople(null,
            new RealtimeVideoStream("peer", "h264", 102, 1280, 720, 30, 4_000_000, null)));
    }

    // The codec setting is a ceiling carried in the passport, not a filter on what we list.
    [Theory]
    [InlineData("Auto", null)]
    [InlineData("UpToH265", "h265")]
    [InlineData("H264", "h264")]
    public void CodecSetting_TravelsAsACeiling(string preferenceName, string? ceiling)
    {
        CallVideoCodecPreference preference = Enum.Parse<CallVideoCodecPreference>(preferenceName);
        RealtimeMediaCapabilities capabilities = CallVideoPreferences.BuildCapabilities(
            "dialog::acct",
            preference,
            new CallVideoResolutionOption(1280, 720),
            CallVideoFrameRatePreference.Auto,
            TestCaptureProbe(),
            [new CallVideoCodecSupport(CallVideoCodecKind.H264, CallVideoRtpConstants.H264PayloadType, true, true, "h264/test")]);
        Assert.Equal(ceiling, capabilities.CodecCeiling);
    }

    // A shared screen goes in its own size, up to 1440p (docs/decisions/per-direction-video-format.md).
    [Theory]
    [InlineData(1920, 1080, 1920, 1080)]
    [InlineData(2560, 1440, 2560, 1440)]
    [InlineData(3840, 2160, 2560, 1440)]
    [InlineData(1366, 768, 1366, 768)]
    public void ScreenShare_DefaultsToItsOwnSizeUpTo1440p(int width, int height, int expectedWidth, int expectedHeight)
    {
        CallVideoResolutionOption picked = CallVideoPreferences.DefaultScreenResolution(
            CallVideoPreferences.BuildResolutionOptions(width, height));
        Assert.Equal(new CallVideoResolutionOption(expectedWidth, expectedHeight), picked);
    }

    // While a share runs the passport says what goes out: the screen in its picked size, whatever
    // the camera is set to. Before, a machine without a camera always said 1280x720.
    [Fact]
    public void Passport_DescribesTheSharedScreenWhileItIsShared()
    {
        RealtimeMediaCapabilities capabilities = CallVideoPreferences.BuildCapabilities(
            "dialog::acct",
            CallVideoCodecPreference.Auto,
            new CallVideoResolutionOption(1280, 720),
            CallVideoFrameRatePreference.Auto,
            TestCaptureProbe(),
            [new CallVideoCodecSupport(CallVideoCodecKind.H264, CallVideoRtpConstants.H264PayloadType, true, true, "h264/test")],
            canShareScreen: true,
            screenContent: true,
            screenResolution: new CallVideoResolutionOption(1920, 1080));
        Assert.Equal("screen", capabilities.Content);
        RealtimeVideoProfileCapability video = capabilities.Video!;
        Assert.True(video.CanSendVideo);
        Assert.Equal((1920u, 1080u, 30u), (video.PreferredWidth, video.PreferredHeight, video.PreferredFps));
        Assert.Equal((1920u, 1080u), (video.MaxWidth, video.MaxHeight));
    }

    [Theory]
    [InlineData(null, "medium")]
    [InlineData(0.10, "low")]
    [InlineData(0.50, "medium")]
    [InlineData(0.90, "high")]
    public void ProcessorLoad_ClassifiesCoarsely(double? busy, string expected)
        => Assert.Equal(expected, ProcessorLoad.Classify(busy));

    [Fact]
    public void ProcessorLoad_ReadsSomething()
        => Assert.Contains(ProcessorLoad.Read(), new[] { "low", "medium", "high" });

    [Fact]
    public void CodecCapabilities_DoNotAdvertiseSoftwareH264ForProduction()
    {
        IReadOnlyList<CallVideoCodecSupport> codecs = CallVideoCodecFactory.GetSupportedCodecs();

        Assert.DoesNotContain(codecs, codec => codec.Kind == CallVideoCodecKind.H264 && codec.EncodeHardware == false);
    }

    [Fact]
    public void NativeWebRtcMediaProbe_ParsesCapabilityJsonWithoutEnablingBackend()
    {
        const string json = """
            {
              "abi_version": 7,
              "available": false,
              "backend": "libwebrtc",
              "transport": "omni_quic_datagram_control",
              "supports_peer_connection": false,
              "supports_rtp_over_omni_quic": false,
              "supports_rtcp_feedback": false,
              "native_session_scaffold": true,
              "native_session_scaffold_codec": "h264",
              "native_session_scaffold_codecs": ["h264"],
              "native_session_nv12_input": true,
              "native_session_bgra_input": true,
              "native_session_bgra_output": true,
              "codec_roundtrip_smoke": true,
              "codec_roundtrip_encoded_bytes": 1234,
              "codec_roundtrip_diagnostics": "h264 encode/decode color-bars ok",
              "hardware_codecs": ["h264"],
              "hardware_h264_probe": {
                "media_foundation_available": true,
                "encoder_count": 1,
                "decoder_count": 1,
                "encoder_names": ["Test HW Encoder"],
                "decoder_names": ["Test HW Decoder"],
                "production_session": true,
                "reason": "test proof"
              },
              "software_codecs": [],
              "reason": "session backend is not implemented yet"
            }
            """;

        NativeWebRtcMediaProbeResult result = NativeWebRtcMediaProbe.ParseProbeJsonForTests(json);

        Assert.True(result.DllLoaded);
        Assert.False(result.Available);
        Assert.Equal("libwebrtc", result.Backend);
        Assert.False(result.SupportsRtpOverOmniQuic);
        Assert.False(result.SupportsRtcpFeedback);
        Assert.True(result.NativeSessionScaffold);
        Assert.Equal("h264", result.NativeSessionScaffoldCodec);
        Assert.Contains("h264", result.NativeSessionScaffoldCodecs);
        Assert.True(result.SupportsNativeSessionCodec(CallVideoCodecKind.H264));
        Assert.True(result.NativeSessionNv12Input);
        Assert.True(result.NativeSessionBgraInput);
        Assert.True(result.NativeSessionBgraOutput);
        Assert.True(result.CodecRoundtripSmoke);
        Assert.Equal(1234ul, result.CodecRoundtripEncodedBytes);
        Assert.Contains("h264", result.HardwareCodecs);
        Assert.True(result.HardwareH264Probe.MediaFoundationAvailable);
        Assert.Equal(1u, result.HardwareH264Probe.EncoderCount);
        Assert.Equal(1u, result.HardwareH264Probe.DecoderCount);
        Assert.True(result.HardwareH264Probe.ProductionSession);
        Assert.Contains("Test HW Encoder", result.HardwareH264Probe.EncoderNames);
        Assert.Contains("not implemented", result.Diagnostics, StringComparison.OrdinalIgnoreCase);
        Assert.Contains("mf_h264_hw", result.Summary, StringComparison.Ordinal);
        Assert.Contains("color-bars ok", result.Diagnostics, StringComparison.OrdinalIgnoreCase);
    }

    [Fact]
    public void NativeWebRtcMediaProbe_MediaFoundationEnumerationDoesNotClaimProductionHardware()
    {
        const string json = """
            {
              "abi_version": 7,
              "available": true,
              "backend": "libwebrtc",
              "transport": "omni_quic_datagram_control",
              "supports_peer_connection": false,
              "supports_rtp_over_omni_quic": true,
              "supports_rtcp_feedback": true,
              "native_session_scaffold": true,
              "native_session_scaffold_codec": "h264",
              "native_session_scaffold_codecs": ["h264"],
              "native_session_nv12_input": true,
              "native_session_bgra_input": true,
              "native_session_bgra_output": true,
              "codec_roundtrip_smoke": true,
              "codec_roundtrip_encoded_bytes": 1234,
              "codec_roundtrip_diagnostics": "h264 ok",
              "hardware_codecs": [],
              "hardware_h264_probe": {
                "media_foundation_available": true,
                "encoder_count": 1,
                "decoder_count": 1,
                "encoder_names": ["Installed HW Encoder"],
                "decoder_names": ["Installed HW Decoder"],
                "production_session": false,
                "reason": "MF MFT exists, production session is still libwebrtc"
              },
              "software_codecs": ["h264"],
              "reason": "ok"
            }
            """;

        NativeWebRtcMediaProbeResult result = NativeWebRtcMediaProbe.ParseProbeJsonForTests(json);

        Assert.Empty(result.HardwareCodecs);
        Assert.True(result.HardwareH264Probe.MediaFoundationAvailable);
        Assert.False(result.HardwareH264Probe.ProductionSession);
        Assert.DoesNotContain("h264", result.HardwareCodecs);
        Assert.Contains("prod:false", result.Summary, StringComparison.Ordinal);
    }

    [Fact]
    public void NativeWebRtcMediaProbe_RuntimeProbeIsNonFatal()
    {
        NativeWebRtcMediaProbeResult result = CallVideoCodecFactory.GetNativeWebRtcProbeResult();

        Assert.False(string.IsNullOrWhiteSpace(result.Backend));
        Assert.False(string.IsNullOrWhiteSpace(result.Diagnostics));
    }

    /// <summary>
    /// Входящее медиа идёт через нативный разборщик, как в звонке: ядро отдаёт ему датаграмму, он
    /// кладёт её приёмнику. Отрицательная половина первая: без приёмника видео не теряется молча,
    /// а превращается в ОДИН запрос «нужен приёмник», и повторная выдача запросов пуста.
    /// </summary>
    [Fact]
    public void NativeWebRtcMediaEngine_RouterAsksForAReceiverThenDeliversVideoToIt()
    {
        if (!ShouldRunNativeSessionTests())
        {
            return;
        }

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateH264Session(
                320,
                180,
                30,
                500_000,
                out NativeWebRtcMediaEngine.Session? sender,
                out string senderDiagnostics),
            senderDiagnostics);
        NativePacketTaps.Attach(sender!);

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateH264Session(
                320,
                180,
                30,
                500_000,
                out NativeWebRtcMediaEngine.Session? receiver,
                out string receiverDiagnostics),
            receiverDiagnostics);
        NativePacketTaps.Attach(receiver!);

        Assert.True(
            NativeWebRtcMediaEngine.MediaRouter.TryCreate(out NativeWebRtcMediaEngine.MediaRouter? router, out string routerDiagnostics),
            routerDiagnostics);

        using (sender)
        using (receiver)
        using (router)
        {
            // The core calls this entry; the test stands in for it.
            RouterDeliver deliver = Marshal.GetDelegateForFunctionPointer<RouterDeliver>(router!.DeliverFunction);
            void Deliver(NativeWebRtcRtpPacketBatch batch)
            {
                foreach (byte[] packet in SplitNativeRtpBatch(batch))
                {
                    GCHandle pinned = GCHandle.Alloc(packet, GCHandleType.Pinned);
                    try
                    {
                        deliver(router.Handle, pinned.AddrOfPinnedObject(), (UIntPtr)packet.Length);
                    }
                    finally
                    {
                        pinned.Free();
                    }
                }
            }

            CreateI420ColorBars(320, 180, out byte[] yPlane, out byte[] uPlane, out byte[] vPlane);
            void PushSameFrame(long timestampUs) => sender!.PushI420Frame(
                yPlane, 320, uPlane, 160, vPlane, 160, 320, 180, timestampUs, forceKeyFrame: true);

            PushSameFrame(1_000_000);
            Deliver(DrainRtpAllowingEncoderLatency(sender!, PushSameFrame));
            Assert.True(router.TakeRequests().VideoReceiverWanted, "video with no receiver must ask for one");
            Assert.False(router.TakeRequests().VideoReceiverWanted, "the request is taken once");

            // The pacer may still be releasing the first frame's tail, which the receiver cannot
            // decode without its head. Keep feeding forced keyframes and handing over everything
            // collected until one decodes — the router's job is to deliver, not to align frames.
            router.SetReceiver(receiver);
            NativeWebRtcDecodedI420Frame decoded = default;
            bool decodedAny = false;
            for (int attempt = 0; attempt < 10 && !decodedAny; attempt++)
            {
                PushSameFrame(2_000_000 + (attempt * 33_333));
                Thread.Sleep(60);
                Deliver(sender!.DrainRtpPackets(256));
                decodedAny = WaitForNativeI420Frame(receiver!, out decoded);
            }

            Assert.True(decodedAny, receiver!.GetDiagnosticsJson());
            Assert.Equal(320, decoded.Width);
            Assert.False(router.TakeRequests().VideoReceiverWanted, "a wired receiver asks for nothing");
            router.SetReceiver(null);
        }
    }

    /// <summary>
    /// Группа: неизвестный отправитель — запрос, и пока его не зарегистрировали, КАЖДЫЙ его пакет
    /// спрашивает снова. На этом стоит отложенная регистрация в оболочке: вступивший слышит других
    /// раньше, чем согласовал свой формат, и регистрировать отправителя без приёмника видео нельзя.
    /// Отрицательная половина — ловушка, в которую попал живой прогон 2026-10-04: зарегистрированного
    /// отправителя маршрутизатор больше не спрашивает никогда, так что регистрация «пока без
    /// видео» оставляла участника без видео на весь звонок.
    /// </summary>
    [Fact]
    public void NativeWebRtcMediaEngine_GroupRouterAsksAgainUntilTheSenderIsWired()
    {
        if (!ShouldRunNativeSessionTests())
        {
            return;
        }

        Assert.False(CallVideoService.GroupSenderCanBeWired(null), "no agreed format — wait");
        Assert.True(CallVideoService.GroupSenderCanBeWired(new RealtimeMediaSelection(
            DialogKey: "group:g",
            SessionId: "media-v2:group:g",
            MaxDatagramBytes: CallVideoRtpConstants.MaxDatagramBytes,
            Video: null,
            SelectedAtUnixMs: 1)));

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateH264Session(320, 180, 30, 500_000, out NativeWebRtcMediaEngine.Session? receiver, out string receiverDiagnostics),
            receiverDiagnostics);
        Assert.True(
            NativeWebRtcMediaEngine.MediaRouter.TryCreate(out NativeWebRtcMediaEngine.MediaRouter? router, out string routerDiagnostics),
            routerDiagnostics);

        using (receiver)
        using (router)
        {
            router!.SetGroupMode(true);
            RouterDeliver deliver = Marshal.GetDelegateForFunctionPointer<RouterDeliver>(router.DeliverFunction);
            byte[] tag = [1, 2, 3, 4, 5, 6, 7, 8];
            // sender_tag ‖ a minimal RTP header (v2, PT 102) — the router routes on the tag only.
            byte[] packet = [.. tag, 0x80, 102, 0, 1, 0, 0, 0, 1, 0, 0, 0, 7];
            void Deliver()
            {
                GCHandle pinned = GCHandle.Alloc(packet, GCHandleType.Pinned);
                try
                {
                    deliver(router.Handle, pinned.AddrOfPinnedObject(), (UIntPtr)packet.Length);
                }
                finally
                {
                    pinned.Free();
                }
            }

            Deliver();
            var wanted = Assert.Single(router.TakeRequests().GroupSenders);
            Assert.Equal(tag, wanted.Tag);
            Assert.Equal(102, wanted.VideoPayloadType);
            Assert.Empty(router.TakeRequests().GroupSenders);
            Deliver();
            Assert.Equal(tag, Assert.Single(router.TakeRequests().GroupSenders).Tag);

            router.SetGroupSender(tag, receiver, levelProbe: null);
            Deliver();
            Assert.Empty(router.TakeRequests().GroupSenders);

            // The sender switched to AV1 (its own pick for the group): asked about once, with the new
            // payload type, so a receiver built for it replaces the H.264 one.
            packet[9] = 45;
            Deliver();
            wanted = Assert.Single(router.TakeRequests().GroupSenders);
            Assert.Equal(45, wanted.VideoPayloadType);
            Deliver();
            Assert.Empty(router.TakeRequests().GroupSenders);
            router.SetGroupSender(tag, null, null);

            // A participant's delivery report about OUR stream comes under the all-0xFF tag: it is
            // feedback for our sending session (counted as such), never a new sender to wire.
            byte[] feedbackTag = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
            packet = [.. feedbackTag, 0x8B, 205, 0, 2, 0, 0, 0, 9, 0, 0, 0, 7];
            ulong feedbackBefore = router.ReadCounters().Feedback;
            Deliver();
            Assert.Equal(feedbackBefore + 1, router.ReadCounters().Feedback);
            Assert.Empty(router.TakeRequests().GroupSenders);
        }
    }

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate void RouterDeliver(IntPtr context, IntPtr packet, UIntPtr packetLength);

    [Fact]
    public void NativeWebRtcMediaEngine_RuntimeRoundtripUsesNativeRtpBatchWhenAvailable()
    {
        if (!ShouldRunNativeSessionTests())
        {
            return;
        }

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateH264Session(
                320,
                180,
                30,
                500_000,
                out NativeWebRtcMediaEngine.Session? sender,
                out string senderDiagnostics),
            senderDiagnostics);
        NativePacketTaps.Attach(sender!);

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateH264Session(
                320,
                180,
                30,
                500_000,
                out NativeWebRtcMediaEngine.Session? receiver,
                out string receiverDiagnostics),
            receiverDiagnostics);
        NativePacketTaps.Attach(receiver!);

        using (sender)
        using (receiver)
        {
            CreateI420ColorBars(320, 180, out byte[] yPlane, out byte[] uPlane, out byte[] vPlane);
            void PushSameFrame(long timestampUs) => sender!.PushI420Frame(
                yPlane,
                320,
                uPlane,
                160,
                vPlane,
                160,
                320,
                180,
                timestampUs,
                forceKeyFrame: true);

            PushSameFrame(1_000_000);

            NativeWebRtcRtpPacketBatch batch = DrainRtpAllowingEncoderLatency(sender!, PushSameFrame);
            Assert.True(batch.Length > 0);
            Assert.True(batch.PacketCount > 0);

            receiver!.PushRtpPackets(batch.Buffer.AsSpan(0, batch.Length), batch.PacketCount);
            Assert.True(WaitForNativeI420Frame(receiver, out NativeWebRtcDecodedI420Frame decoded));
            Assert.Equal(320, decoded.Width);
            Assert.Equal(180, decoded.Height);
            Assert.True(DecodedI420ColorBarsArePlausible(decoded.Buffer, decoded.Width, decoded.Height));

            string diagnostics = receiver.GetDiagnosticsJson();
            Assert.Contains("\"native_session_scaffold\":true", diagnostics, StringComparison.Ordinal);
            Assert.Contains("\"rtp_frames_assembled\":1", diagnostics, StringComparison.Ordinal);
        }
    }

    /// <summary>
    /// Отчёт о доставке (RFC 8888) обязан быть ПОСТРОЕН приёмником, уйти тем же путём, что его
    /// остальной RTCP, и опознаваться там тем же признаком 205/11, которым его разбирает приём.
    ///
    /// Отрицательная половина здесь первая и не декоративная: сессия, которая не приняла ни
    /// одного пакета, должна показывать НОЛЬ при живом генераторе. Без неё счётчик, привязанный
    /// по ошибке к самому факту создания генератора, прошёл бы проверку.
    ///
    /// Зачем всё это: пока наружу были видны только счётчики оболочки на концах цепочки, ноль на
    /// них одинаково означал «генератор молчит», «построил, но не забрали», «забрали, но не
    /// доехало» и «доехало, но не разобралось». Первая попытка перевести отчёт на быстрый путь
    /// встала ровно на этой неразличимости и была откачена без диагноза
    /// (docs/bugs/datagrams-we-counted-as-sent-never-left.md).
    /// </summary>
    [Fact]
    public void NativeWebRtcMediaEngine_DeliveryReportsRideTheReceiversOwnRtcpAndAreRecognizableThere()
    {
        if (!ShouldRunNativeSessionTests())
        {
            return;
        }

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateH264Session(
                320,
                180,
                30,
                500_000,
                out NativeWebRtcMediaEngine.Session? sender,
                out string senderDiagnostics),
            senderDiagnostics);
        NativePacketTaps.Attach(sender!);

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateH264Session(
                320,
                180,
                30,
                500_000,
                out NativeWebRtcMediaEngine.Session? receiver,
                out string receiverDiagnostics),
            receiverDiagnostics);
        NativePacketTaps.Attach(receiver!);

        using (sender)
        using (receiver)
        {
            string silent = receiver!.GetDiagnosticsJson();
            Assert.Equal(1, ReadDiagnosticsInt(silent, "\"feedback_generator_ready\":"));
            Assert.Equal(0, ReadDiagnosticsInt(silent, "\"transport_feedback_reports_built\":"));
            Assert.Equal(0, ReadDiagnosticsInt(silent, "\"rtcp_packets_sent\":"));
            Assert.Equal(0u, receiver.DrainRtpPackets(32).PacketCount);

            CreateI420ColorBars(320, 180, out byte[] yPlane, out byte[] uPlane, out byte[] vPlane);
            long timestampUs = 1_000_000;
            void PushFrame(long at) => sender!.PushI420Frame(
                yPlane, 320, uPlane, 160, vPlane, 160, 320, 180, at, forceKeyFrame: false);

            // Генератор отдаёт отчёт по своему таймеру (минимальный интервал — десятки
            // миллисекунд), поэтому одной пачки мало: кормим приёмник, пока он не построит
            // отчёт, но не дольше разумного предела.
            long built = 0;
            for (int round = 0; round < 40 && built <= 0; round++)
            {
                PushFrame(timestampUs);
                timestampUs += 33_000;
                NativeWebRtcRtpPacketBatch batch = sender!.DrainRtpPackets(128);
                if (batch.PacketCount > 0)
                {
                    receiver.PushRtpPackets(batch.Buffer.AsSpan(0, batch.Length), batch.PacketCount);
                }

                Thread.Sleep(40);
                built = ReadDiagnosticsInt(
                    receiver.GetDiagnosticsJson(), "\"transport_feedback_reports_built\":");
            }

            string fed = receiver.GetDiagnosticsJson();
            Assert.True(built > 0, $"receiver took packets and built no delivery report: {fed}");

            // ОТЧЁТ ЛЕЖИТ В ОБЩЕЙ ОЧЕРЕДИ RTCP И УЗНАЁТСЯ ТЕМ ЖЕ ПРИЗНАКОМ, ЧТО В ОБОЛОЧКЕ.
            //
            // Своего буфера у отчёта больше нет: он едет тем же быстрым путём, что остальной
            // RTCP приёмника, а на той стороне его разбирает демукс по типу 205 и формату 11.
            // Признак проверяется здесь ровно тот же — иначе «уехало» и «узнали» разъедутся, а
            // именно на этом встала откаченная попытка: в ней ни одно число не отличало
            // «не уехало» от «уехало неузнанным».
            Assert.True(
                ReadDiagnosticsInt(fed, "\"rtcp_packets_sent\":") > 0,
                $"a report was built but nothing was handed to the sink: {fed}");

            int feedbackPackets = 0;
            int drainedPackets = 0;
            for (int round = 0; round < 8; round++)
            {
                NativeWebRtcRtpPacketBatch back = receiver.DrainRtpPackets(32);
                if (back.PacketCount == 0)
                {
                    break;
                }

                drainedPackets += (int)back.PacketCount;
                ReadOnlySpan<byte> span = back.Buffer.AsSpan(0, back.Length);
                int offset = 0;
                for (uint i = 0; i < back.PacketCount && offset + 2 <= span.Length; i++)
                {
                    int length = BinaryPrimitives.ReadUInt16LittleEndian(span[offset..]);
                    offset += 2;
                    if (length == 0 || offset + length > span.Length)
                    {
                        break;
                    }

                    ReadOnlySpan<byte> packet = span.Slice(offset, length);
                    offset += length;
                    if (packet.Length >= 2 && packet[1] == 205 && (packet[0] & 0x1F) == 11)
                    {
                        feedbackPackets++;
                    }
                }
            }

            Assert.True(
                feedbackPackets > 0,
                $"the RTCP queue held {drainedPackets} packets and none of them was a delivery "
                    + $"report by the shell's own 205/11 test: {fed}");

            // СЧЁТ СХОДИТСЯ ПОШТУЧНО: всё, что сессия отдала, получатель принял, и ничего не ушло
            // мимо. Очереди, у которой было «положено / забрано / вытеснено», больше нет — отдача
            // сразу в получателя (docs/bugs/media-rode-the-shells-timer.md). Отдано ≥ собрано
            // тестом: генератор мог выпустить ещё отчёт после последней выборки.
            string drainedJson = receiver.GetDiagnosticsJson();
            long sentTotal = ReadDiagnosticsInt(drainedJson, "\"rtcp_packets_sent\":");
            Assert.True(
                sentTotal >= drainedPackets,
                $"the tap collected {drainedPackets} packets but the session reports {sentTotal}: {drainedJson}");
            Assert.Equal(0, ReadDiagnosticsInt(drainedJson, "\"packets_unrouted\":"));
        }
    }

    [Fact]
    public void NativeWebRtcMediaEngine_RuntimeRoundtripPreservesNv12ToBgraColorsWhenAvailable()
    {
        if (!ShouldRunNativeSessionTests())
        {
            return;
        }

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateH264Session(
                320,
                180,
                30,
                500_000,
                out NativeWebRtcMediaEngine.Session? sender,
                out string senderDiagnostics),
            senderDiagnostics);
        NativePacketTaps.Attach(sender!);

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateH264Session(
                320,
                180,
                30,
                500_000,
                out NativeWebRtcMediaEngine.Session? receiver,
                out string receiverDiagnostics),
            receiverDiagnostics);
        NativePacketTaps.Attach(receiver!);

        using (sender)
        using (receiver)
        {
            CreateNv12ColorBars(320, 180, out byte[] yPlane, out byte[] uvPlane);
            void PushSameFrame(long timestampUs) => sender!.PushNv12Frame(
                yPlane,
                320,
                uvPlane,
                320,
                320,
                180,
                timestampUs,
                forceKeyFrame: true);

            PushSameFrame(2_000_000);

            NativeWebRtcRtpPacketBatch batch = DrainRtpAllowingEncoderLatency(sender!, PushSameFrame);
            Assert.True(batch.Length > 0);
            Assert.True(batch.PacketCount > 0);

            receiver!.PushRtpPackets(batch.Buffer.AsSpan(0, batch.Length), batch.PacketCount);
            Assert.True(WaitForNativeBgraFrame(receiver, out NativeWebRtcDecodedBgraFrame decoded));
            Assert.Equal(320, decoded.Width);
            Assert.Equal(180, decoded.Height);
            Assert.Equal(checked(320 * 180 * 4), decoded.Buffer.Length);
            AssertDominantColor(decoded.Buffer, decoded.Width, decoded.Height, decoded.Width / 6, decoded.Height / 2, DominantColor.Red);
            AssertDominantColor(decoded.Buffer, decoded.Width, decoded.Height, decoded.Width / 2, decoded.Height / 2, DominantColor.Green);
            AssertDominantColor(decoded.Buffer, decoded.Width, decoded.Height, decoded.Width * 5 / 6, decoded.Height / 2, DominantColor.Blue);
        }
    }

    [Fact]
    public void NativeWebRtcMediaEngine_RuntimeRoundtripPreservesBgraColorsWhenAvailable()
    {
        if (!ShouldRunNativeSessionTests())
        {
            return;
        }

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateH264Session(
                320,
                180,
                30,
                500_000,
                out NativeWebRtcMediaEngine.Session? sender,
                out string senderDiagnostics),
            senderDiagnostics);
        NativePacketTaps.Attach(sender!);

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateH264Session(
                320,
                180,
                30,
                500_000,
                out NativeWebRtcMediaEngine.Session? receiver,
                out string receiverDiagnostics),
            receiverDiagnostics);
        NativePacketTaps.Attach(receiver!);

        using (sender)
        using (receiver)
        {
            byte[] bgra = CreateBgraColorBarsFrame(320, 180);
            void PushSameFrame(long timestampUs) => sender!.PushBgraFrame(
                bgra,
                320 * 4,
                320,
                180,
                timestampUs,
                forceKeyFrame: true);

            PushSameFrame(3_000_000);

            NativeWebRtcRtpPacketBatch batch = DrainRtpAllowingEncoderLatency(sender!, PushSameFrame);
            Assert.True(batch.Length > 0);
            Assert.True(batch.PacketCount > 0);

            receiver!.PushRtpPackets(batch.Buffer.AsSpan(0, batch.Length), batch.PacketCount);
            Assert.True(WaitForNativeBgraFrame(receiver, out NativeWebRtcDecodedBgraFrame decoded));
            Assert.Equal(320, decoded.Width);
            Assert.Equal(180, decoded.Height);
            Assert.Equal(checked(320 * 180 * 4), decoded.Buffer.Length);
            AssertDominantColor(decoded.Buffer, decoded.Width, decoded.Height, decoded.Width / 6, decoded.Height / 2, DominantColor.Red);
            AssertDominantColor(decoded.Buffer, decoded.Width, decoded.Height, decoded.Width / 2, decoded.Height / 2, DominantColor.Green);
            AssertDominantColor(decoded.Buffer, decoded.Width, decoded.Height, decoded.Width * 5 / 6, decoded.Height / 2, DominantColor.Blue);
        }
    }

    [Fact]
    public void NativeWebRtcMediaEngine_RuntimeSessionAppliesRateAndKeyframeRequestsWhenAvailable()
    {
        if (!ShouldRunNativeSessionTests())
        {
            return;
        }

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateH264Session(
                320,
                180,
                30,
                500_000,
                out NativeWebRtcMediaEngine.Session? session,
                out string diagnostics),
            diagnostics);
        NativePacketTaps.Attach(session!);

        using (session)
        {
            session!.SetRates(250_000, 15);
            session.RequestKeyFrame();
            CreateI420ColorBars(320, 180, out byte[] yPlane, out byte[] uPlane, out byte[] vPlane);
            session.PushI420Frame(
                yPlane,
                320,
                uPlane,
                160,
                vPlane,
                160,
                320,
                180,
                4_000_000,
                forceKeyFrame: false);

            string nativeDiagnostics = session.GetDiagnosticsJson();
            Assert.Contains("\"target_bitrate_bps\":250000", nativeDiagnostics, StringComparison.Ordinal);
            Assert.Contains("\"fps\":15", nativeDiagnostics, StringComparison.Ordinal);
            Assert.Contains("\"rate_updates\":1", nativeDiagnostics, StringComparison.Ordinal);
            Assert.Contains("\"keyframe_requests\":1", nativeDiagnostics, StringComparison.Ordinal);
        }
    }

    [Fact]
    public void NativeWebRtcMediaEngine_ARealRtcpNackGetsResentWhenAvailable()
    {
        if (!ShouldRunNativeSessionTests())
        {
            return;
        }

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateH264Session(
                320,
                180,
                30,
                500_000,
                out NativeWebRtcMediaEngine.Session? sender,
                out string senderDiagnostics),
            senderDiagnostics);
        NativePacketTaps.Attach(sender!);

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateH264Session(
                320,
                180,
                30,
                500_000,
                out NativeWebRtcMediaEngine.Session? receiver,
                out string receiverDiagnostics),
            receiverDiagnostics);
        NativePacketTaps.Attach(receiver!);

        using (sender)
        using (receiver)
        {
            CreateI420ColorBars(320, 180, out byte[] yPlane, out byte[] uPlane, out byte[] vPlane);
            void PushSameFrame(long timestampUs) => sender!.PushI420Frame(
                yPlane,
                320,
                uPlane,
                160,
                vPlane,
                160,
                320,
                180,
                timestampUs,
                forceKeyFrame: true);

            PushSameFrame(5_000_000);

            NativeWebRtcRtpPacketBatch batch = DrainRtpAllowingEncoderLatency(sender!, PushSameFrame);
            Assert.True(batch.Length > 0);
            Assert.True(batch.PacketCount > 0);
            ushort firstSequence = ReadFirstNativeRtpSequence(batch.Buffer.AsSpan(0, batch.Length));
            uint mediaSsrc = ReadFirstNativeRtpSsrc(batch.Buffer.AsSpan(0, batch.Length));
            receiver!.PushRtpPackets(batch.Buffer.AsSpan(0, batch.Length), batch.PacketCount);

            // THE PEER'S REQUEST ARRIVES AS REAL RTCP (ABI 16) — exactly how the native router hands
            // it to the sending session: a generic NACK (RFC 4585) pushed in with the media path.
            // The retransmission itself leaves on the RTX stream under its own numbering, so it is
            // checked by the sender's own count of what it resent, not by sequence equality.
            //
            // Negative half first: a NACK naming a stream that is not this sender's resends nothing.
            long RetransmissionsAfter(int polls)
            {
                long sent = 0;
                for (int attempt = 0; attempt < polls; attempt++)
                {
                    sender.DrainRtpPackets(maxPackets: 8);
                    sent = ReadDiagnosticsInt(sender.GetDiagnosticsJson(), "\"module_retransmissions_sent\":");
                    if (sent > 0)
                    {
                        break;
                    }

                    Thread.Sleep(10);
                }

                return sent;
            }

            byte[] foreignNack = BuildGenericNackBatch(mediaSsrc ^ 1u, firstSequence);
            sender.PushRtpPackets(foreignNack, 1);
            Assert.Equal(0, RetransmissionsAfter(20));

            byte[] nack = BuildGenericNackBatch(mediaSsrc, firstSequence);
            sender.PushRtpPackets(nack, 1);
            long retransmissionsSent = RetransmissionsAfter(200);
            Assert.True(
                retransmissionsSent > 0,
                $"peer asked for a retransmission and none left the sender: {sender.GetDiagnosticsJson()}");
        }
    }

    [Fact]
    public void NativeWebRtcMediaEngine_AbrBacksOffWhenThePeerStopsReporting()
    {
        // Guards docs/bugs/call-drowns-the-mobile-uplink.md. A silent peer used to mean ABR was
        // never even called, so the sender held its full target while the path it was drowning had
        // no way to recover. Every OTHER input here says "all is well" — nothing lost, no queue,
        // low RTT — precisely so that the staleness is the only thing that can cause the drop.
        if (!ShouldRunNativeSessionTests())
        {
            return;
        }

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateSession(
                CallVideoCodecKind.H264,
                1280,
                720,
                30,
                4_000_000,
                out NativeWebRtcMediaEngine.Session? session,
                out string diagnostics),
            diagnostics);
        NativePacketTaps.Attach(session!);

        using (session)
        {
            // Negative half first: silence from a peer we have sent NOTHING to means nothing.
            // The age of feedback is counted from our first sent packet (ABI 18, native).
            NativeWebRtcMediaAbrTarget idle = session!.UpdateAdaptiveBitrate(new NativeWebRtcMediaAbrSample(1_000));
            idle = session.UpdateAdaptiveBitrate(new NativeWebRtcMediaAbrSample(4_000));
            Assert.Equal(4_000_000, idle.BitrateBps);

            SendOneFrame(session, 1280, 720);
            session.UpdateAdaptiveBitrate(new NativeWebRtcMediaAbrSample(5_000));
            NativeWebRtcMediaAbrTarget quiet = session.UpdateAdaptiveBitrate(new NativeWebRtcMediaAbrSample(7_500));

            Assert.True(quiet.Changed, "silence from the peer we are sending to must move the target");
            Assert.True(
                quiet.BitrateBps < 4_000_000,
                $"expected a backoff, got {quiet.BitrateBps} bps");
            Assert.Equal(NativeWebRtcMediaAbrState.DownCongested, quiet.State);
        }
    }

    [Fact]
    public void NativeWebRtcMediaEngine_RuntimeAbrDropsFastAndRecoversAfterStableWindowWhenAvailable()
    {
        if (!ShouldRunNativeSessionTests())
        {
            return;
        }

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateSession(
                CallVideoCodecKind.H264,
                1920,
                1080,
                30,
                4_000_000,
                out NativeWebRtcMediaEngine.Session? session,
                out string diagnostics),
            diagnostics);
        NativePacketTaps.Attach(session!);

        using (session)
        {
            // ЗАТОР ЗАДАЁТСЯ МОЛЧАНИЕМ ПИРА, А НЕ ДОЛЕЙ ПОТЕРЬ (#57, шаг C2b).
            //
            // Доля потерь, RTT и джиттер из этой выборки больше никуда не ведут: канал считает
            // RtpTransportControllerSend по ФАКТИЧЕСКОЙ доставке, то есть по отчётам собеседника.
            // В этом тесте не отправляется ни одного пакета и нет второй стороны — значит и
            // отчётов нет. Единственный сигнал затора, доходящий до решения без собеседника, —
            // «обратная связь замолчала»; он же и самый тяжёлый по последствиям
            // (docs/bugs/call-drowns-the-mobile-uplink.md).
            //
            // С ABI 18 молчание не подаётся выборкой, а наступает: один кадр уходит, отчётов нет, и
            // давность обратной связи растёт от первого отправленного пакета сама.
            SendOneFrame(session!, 1920, 1080);
            session!.UpdateAdaptiveBitrate(new NativeWebRtcMediaAbrSample(1_000));
            NativeWebRtcMediaAbrTarget downTarget = session.UpdateAdaptiveBitrate(new NativeWebRtcMediaAbrSample(4_000));

            Assert.True(downTarget.Changed);
            Assert.Equal(NativeWebRtcMediaAbrState.DownCongested, downTarget.State);
            Assert.True(downTarget.BitrateBps < 4_000_000);
            // ЧАСТОТА КАДРОВ И РАЗМЕР ЗДЕСЬ НЕ ДВИГАЮТСЯ — И ЭТО ПРАВИЛЬНО (#61, шаг D3).
            //
            // Раньше здесь стояло `Fps < 30`: частота была нашей ЧИСТОЙ ФУНКЦИЕЙ от битрейта, и
            // одного упавшего битрейта хватало, чтобы её срезать. Теперь рабочую точку по размеру
            // и частоте выбирает VideoStreamEncoderResourceManager внутри движка, и выбирает по
            // ФАКТУ — по тому, сколько кодировщик отдал на самом деле и успевает ли он.
            //
            // В этом тесте подаётся один кадр — судить по нему не о чем, пожеланий источнику нет, и
            // обе величины обязаны остаться такими, какими их выбрал отправитель. Требовать здесь снижения значило бы требовать решения, принятого без
            // единого измерения, — ровно того, чем и была снятая лестница.
            //
            // Само снижение проверяется там, где для него есть вход: нативный стенд
            // `RunAbrResolutionLadderTest` подаёт кадры и требует монотонного спуска (замер:
            // 1280 -> 990 -> 766 -> 592).
            Assert.Equal(30, downTarget.Fps);
            Assert.Equal(1920, downTarget.Width);
            Assert.Equal(1080, downTarget.Height);

            // Отчётов так и нет: дальше цель может только стоять или падать. «Стабильного окна» по
            // словам вызывающего больше нет — свежесть обратной связи сессия знает сама.
            int previousBitrate = downTarget.BitrateBps;
            for (int i = 0; i < 3; i++)
            {
                NativeWebRtcMediaAbrTarget next = session.UpdateAdaptiveBitrate(new NativeWebRtcMediaAbrSample(5_000 + (i * 1_000)));
                Assert.True(next.BitrateBps <= previousBitrate, $"{previousBitrate} -> {next.BitrateBps}");
                previousBitrate = next.BitrateBps;
            }

            // ЗДЕСЬ ПРОВЕРЯЛСЯ ПОДЪЁМ ПО ТАЙМЕРУ. ТЕПЕРЬ ПРОВЕРЯЕТСЯ ОБРАТНОЕ — И ЭТО НЕ ОСЛАБЛЕНИЕ.
            //
            // Было: после «стабильного окна» цель обязана вырасти, состояние up:stable. Это форма
            // самописной лестницы, которую сняли 2026-08-22: она поднимала себя по часам, не имея
            // ни одного доказательства, что путь снова несёт. В этом тесте пакеты не отправляются
            // вовсе, потери нулевые, QueueDrops не растёт — то есть доказательств нет никаких, а
            // подъём всё равно происходил.
            //
            // Единственный регулятор скорости теперь GoogCC, и он так не умеет: отчёт «всё дошло»
            // уходит в него только когда в интервале что-то ушло, иначе доле потерь не с чем
            // соотноситься. Ноль отправленных и ноль потерянных — это не «канал здоров», это «мы
            // молчали». Разгон на таком основании и есть возврат в затор вслепую.
            //
            // Поэтому фиксируем новый инвариант: БЕЗ ДОКАЗАТЕЛЬСТВ ДОСТАВКИ ЦЕЛЬ НЕ РАСТЁТ. Это
            // защита от того, чтобы кто-нибудь снова завёл подъём по таймеру.
            //
            // Настоящее восстановление проверяется там, где есть реальная отправка и реальная
            // обратная связь: RunAbrResolutionLadderTest и RunBufferbloatFeedbackSmoke в нативном
            // наборе (native/omni-webrtc-media/tests/omni_webrtc_media_smoke.cpp).
            NativeWebRtcMediaAbrTarget quietTarget = session.UpdateAdaptiveBitrate(new NativeWebRtcMediaAbrSample(9_100));
            Assert.True(
                quietTarget.BitrateBps <= downTarget.BitrateBps,
                $"target climbed to {quietTarget.BitrateBps} bps without any evidence of delivery");
            Assert.NotEqual(NativeWebRtcMediaAbrState.UpStable, quietTarget.State);

            string nativeDiagnostics = session.GetDiagnosticsJson();
            Assert.Contains("\"abr_target_bitrate_bps\":", nativeDiagnostics, StringComparison.Ordinal);
        }
    }

    [Fact]
    public void NativeWebRtcMediaEngine_RuntimeAbrPreservesNegotiatedCapsAcrossAdaptiveReconfigureWhenAvailable()
    {
        if (!ShouldRunNativeSessionTests())
        {
            return;
        }

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateSession(
                CallVideoCodecKind.H264,
                1280,
                720,
                24,
                2_600_000,
                out NativeWebRtcMediaEngine.Session? session,
                out string diagnostics,
                negotiatedWidth: 1920,
                negotiatedHeight: 1080,
                negotiatedFps: 30,
                negotiatedBitrateBps: 4_000_000),
            diagnostics);
        NativePacketTaps.Attach(session!);

        using (session)
        {
            NativeWebRtcMediaAbrTarget target = default;
            for (int i = 0; i < 8; i++)
            {
                target = session!.UpdateAdaptiveBitrate(CreateStableAbrSample(1_000 + (i * 4_200)));
            }

            // ПОТОЛКИ, А НЕ ПОДЪЁМ.
            //
            // Здесь стояло `Assert.Equal(UpStable)` и требование, чтобы цель ПЕРЕРОСЛА 2.6 Мбит/с.
            // Проверялось это подъёмом по таймеру — свойством самописной лестницы, снятой
            // 2026-08-22. В этом тесте нет ни отправки, ни отчётов о доставке, поэтому GoogCC —
            // теперь единственный регулятор — правомерно не поднимает ничего: доказательств, что
            // канал несёт больше, ему никто не дал.
            //
            // Требование теста при этом НЕ отменяется: потолки берутся из СОГЛАСОВАННОГО профиля
            // (30 к/с, 4 Мбит/с), а не из размера, с которым создана сессия. Просто исполнитель у
            // него теперь другой — это `constraints.max_data_rate`, заданный регулятору при
            // создании, и он проверяется структурно: рабочая точка обязана оставаться под
            // согласованным профилем и не имеет права быть прибитой к размеру создания.
            Assert.NotEqual(NativeWebRtcMediaAbrState.UpStable, target.State);
            Assert.True(target.Fps <= 30, $"fps {target.Fps} climbed above the negotiated profile");
            Assert.True(
                target.BitrateBps <= 4_000_000,
                $"bitrate {target.BitrateBps} climbed above the negotiated profile");
            // Размер кадра подчиняется ТОМУ ЖЕ правилу, что частота и битрейт: потолок — из
            // согласованного профиля. Здесь стояло обратное — «размер остаётся тем, с которым
            // сессию создали», — и это была утренняя правка 2026-08-17, снявшая адаптацию
            // разрешения целиком. К вечеру выяснилось, чем она обошлась: у H.264 кадр 1280x720
            // начинается от 900 кбит/с, а мы просили у него 150 и считали виноватым кодировщик.
            //
            // Проверяем именно потолок, а не конкретную ступень: ступени берутся из таблицы
            // апстрима и от неё же зависят, а инвариант наш — выше согласованного не подниматься
            // никогда.
            Assert.True(
                target.Width >= 1280,
                $"на чистом канале разрешение обязано отыграть вверх, а осталось {target.Width}");
            Assert.True(
                target.Width <= 1920 && target.Height <= 1080,
                $"разрешение ушло выше согласованного потолка: {target.Width}x{target.Height}");
        }
    }

    [Fact]
    public void NativeWebRtcMediaEngine_RuntimeH264RoundtripUsesNativeRtpBatchWhenAdvertised()
    {
        if (!ShouldRunNativeSessionTests())
        {
            return;
        }

        NativeWebRtcMediaProbeResult probe = CallVideoCodecFactory.GetNativeWebRtcProbeResult();
        if (!probe.SupportsNativeSessionCodec(CallVideoCodecKind.H264))
        {
            return;
        }

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateSession(
                CallVideoCodecKind.H264,
                320,
                180,
                30,
                500_000,
                out NativeWebRtcMediaEngine.Session? sender,
                out string senderDiagnostics),
            senderDiagnostics);
        NativePacketTaps.Attach(sender!);

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateSession(
                CallVideoCodecKind.H264,
                320,
                180,
                30,
                500_000,
                out NativeWebRtcMediaEngine.Session? receiver,
                out string receiverDiagnostics),
            receiverDiagnostics);
        NativePacketTaps.Attach(receiver!);

        using (sender)
        using (receiver)
        {
            CreateI420ColorBars(320, 180, out byte[] yPlane, out byte[] uPlane, out byte[] vPlane);
            void PushSameFrame(long timestampUs) => sender!.PushI420Frame(
                yPlane,
                320,
                uPlane,
                160,
                vPlane,
                160,
                320,
                180,
                timestampUs,
                forceKeyFrame: true);

            PushSameFrame(1_000_000);

            NativeWebRtcRtpPacketBatch batch = DrainRtpAllowingEncoderLatency(sender!, PushSameFrame);
            Assert.True(batch.Length > 0);
            Assert.True(batch.PacketCount > 0);

            receiver!.PushRtpPackets(batch.Buffer.AsSpan(0, batch.Length), batch.PacketCount);
            Assert.True(WaitForNativeI420Frame(receiver, out NativeWebRtcDecodedI420Frame decoded));
            Assert.Equal(320, decoded.Width);
            Assert.Equal(180, decoded.Height);
            Assert.True(DecodedI420ColorBarsArePlausible(decoded.Buffer, decoded.Width, decoded.Height));

            string diagnostics = receiver.GetDiagnosticsJson();
            Assert.Contains("\"codec\":\"h264\"", diagnostics, StringComparison.Ordinal);
            Assert.Contains("\"rtp_frames_assembled\":1", diagnostics, StringComparison.Ordinal);
        }
    }

    [Fact]
    public void NativeWebRtcMediaEngine_H264ReceiverAcceptsLowerActualCaptureResolutionWhenAdvertised()
    {
        if (!ShouldRunNativeSessionTests())
        {
            return;
        }

        NativeWebRtcMediaProbeResult probe = CallVideoCodecFactory.GetNativeWebRtcProbeResult();
        if (!probe.SupportsNativeSessionCodec(CallVideoCodecKind.H264))
        {
            return;
        }

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateSession(
                CallVideoCodecKind.H264,
                320,
                180,
                30,
                500_000,
                out NativeWebRtcMediaEngine.Session? sender,
                out string senderDiagnostics,
                negotiatedWidth: 1280,
                negotiatedHeight: 720,
                negotiatedFps: 30,
                negotiatedBitrateBps: 4_000_000),
            senderDiagnostics);
        NativePacketTaps.Attach(sender!);

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateSession(
                CallVideoCodecKind.H264,
                1280,
                720,
                30,
                4_000_000,
                out NativeWebRtcMediaEngine.Session? receiver,
                out string receiverDiagnostics,
                negotiatedWidth: 1280,
                negotiatedHeight: 720,
                negotiatedFps: 30,
                negotiatedBitrateBps: 4_000_000),
            receiverDiagnostics);
        NativePacketTaps.Attach(receiver!);

        using (sender)
        using (receiver)
        {
            CreateNv12ColorBars(320, 180, out byte[] yPlane, out byte[] uvPlane);
            void PushSameFrame(long timestampUs) => sender!.PushNv12Frame(
                yPlane,
                320,
                uvPlane,
                320,
                320,
                180,
                timestampUs,
                forceKeyFrame: true);

            PushSameFrame(1_000_000);

            NativeWebRtcRtpPacketBatch batch = DrainRtpAllowingEncoderLatency(sender!, PushSameFrame);
            Assert.True(batch.Length > 0);
            Assert.True(batch.PacketCount > 0);

            receiver!.PushRtpPackets(batch.Buffer.AsSpan(0, batch.Length), batch.PacketCount);
            Assert.True(WaitForNativeBgraFrame(receiver, out NativeWebRtcDecodedBgraFrame decoded));
            Assert.Equal(320, decoded.Width);
            Assert.Equal(180, decoded.Height);
            AssertDominantColor(decoded.Buffer, decoded.Width, decoded.Height, decoded.Width / 6, decoded.Height / 2, DominantColor.Red);
            AssertDominantColor(decoded.Buffer, decoded.Width, decoded.Height, decoded.Width / 2, decoded.Height / 2, DominantColor.Green);
            AssertDominantColor(decoded.Buffer, decoded.Width, decoded.Height, decoded.Width * 5 / 6, decoded.Height / 2, DominantColor.Blue);
        }
    }

    [Fact]
    public void NativeWebRtcMediaEngine_H264ReceiverAssemblesFramesFromIndividualRtpPacketsWhenAdvertised()
    {
        if (!ShouldRunNativeSessionTests())
        {
            return;
        }

        NativeWebRtcMediaProbeResult probe = CallVideoCodecFactory.GetNativeWebRtcProbeResult();
        if (!probe.SupportsNativeSessionCodec(CallVideoCodecKind.H264))
        {
            return;
        }

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateSession(
                CallVideoCodecKind.H264,
                320,
                180,
                30,
                500_000,
                out NativeWebRtcMediaEngine.Session? sender,
                out string senderDiagnostics),
            senderDiagnostics);
        NativePacketTaps.Attach(sender!);

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateSession(
                CallVideoCodecKind.H264,
                320,
                180,
                30,
                500_000,
                out NativeWebRtcMediaEngine.Session? receiver,
                out string receiverDiagnostics),
            receiverDiagnostics);
        NativePacketTaps.Attach(receiver!);

        using (sender)
        using (receiver)
        {
            CreateNv12ColorBars(320, 180, out byte[] yPlane, out byte[] uvPlane);
            void PushSameFrame(long timestampUs) => sender!.PushNv12Frame(
                yPlane,
                320,
                uvPlane,
                320,
                320,
                180,
                timestampUs,
                forceKeyFrame: true);

            PushSameFrame(1_000_000);

            NativeWebRtcRtpPacketBatch batch = DrainRtpAllowingEncoderLatency(sender!, PushSameFrame);
            Assert.True(batch.Length > 0);
            Assert.True(batch.PacketCount > 0);

            foreach (byte[] packet in SplitNativeRtpBatch(batch))
            {
                receiver!.PushRtpPackets(PackSingleNativeRtpPacket(packet), 1);
            }

            Assert.True(WaitForNativeBgraFrame(receiver!, out NativeWebRtcDecodedBgraFrame decoded));
            Assert.Equal(320, decoded.Width);
            Assert.Equal(180, decoded.Height);
        }
    }

    [Fact]
    public void NativeWebRtcMediaEngine_H264ForcedKeyframesCarryParameterSetsForFreshReceiver()
    {
        if (!ShouldRunNativeSessionTests())
        {
            return;
        }

        NativeWebRtcMediaProbeResult probe = CallVideoCodecFactory.GetNativeWebRtcProbeResult();
        if (!probe.SupportsNativeSessionCodec(CallVideoCodecKind.H264))
        {
            return;
        }

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateSession(
                CallVideoCodecKind.H264,
                320,
                180,
                30,
                500_000,
                out NativeWebRtcMediaEngine.Session? sender,
                out string senderDiagnostics),
            senderDiagnostics);
        NativePacketTaps.Attach(sender!);

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateSession(
                CallVideoCodecKind.H264,
                320,
                180,
                30,
                500_000,
                out NativeWebRtcMediaEngine.Session? warmReceiver,
                out string warmReceiverDiagnostics),
            warmReceiverDiagnostics);
        NativePacketTaps.Attach(warmReceiver!);

        Assert.True(
            NativeWebRtcMediaEngine.TryCreateSession(
                CallVideoCodecKind.H264,
                320,
                180,
                30,
                500_000,
                out NativeWebRtcMediaEngine.Session? freshReceiver,
                out string freshReceiverDiagnostics),
            freshReceiverDiagnostics);
        NativePacketTaps.Attach(freshReceiver!);

        using (sender)
        using (warmReceiver)
        using (freshReceiver)
        {
            CreateNv12ColorBars(320, 180, out byte[] yPlane, out byte[] uvPlane);
            NativeWebRtcRtpPacketBatch firstKeyframe = PushNv12FrameAndDrainRtp(
                sender!,
                yPlane,
                uvPlane,
                timestampUs: 1_000_000,
                forceKeyFrame: true);
            Assert.True(firstKeyframe.PacketCount > 0, sender!.GetDiagnosticsJson());
            warmReceiver!.PushRtpPackets(firstKeyframe.Buffer.AsSpan(0, firstKeyframe.Length), firstKeyframe.PacketCount);
            Assert.True(
                WaitForNativeBgraFrame(warmReceiver, out _),
                warmReceiver.GetDiagnosticsJson());

            bool decodedFreshForcedKeyframe = false;
            for (int attempt = 0; attempt < 6 && !decodedFreshForcedKeyframe; attempt++)
            {
                NativeWebRtcRtpPacketBatch forcedKeyframe = PushNv12FrameAndDrainRtp(
                    sender!,
                    yPlane,
                    uvPlane,
                    timestampUs: 2_000_000 + (attempt * 33_333),
                    forceKeyFrame: true);
                Assert.True(forcedKeyframe.PacketCount > 0);

                foreach (byte[] packet in SplitNativeRtpBatch(forcedKeyframe))
                {
                    freshReceiver!.PushRtpPackets(PackSingleNativeRtpPacket(packet), 1);
                }

                decodedFreshForcedKeyframe = WaitForNativeBgraFrame(freshReceiver!, out _);
            }

            Assert.True(
                decodedFreshForcedKeyframe,
                $"Forced H.264 keyframes must be independently decodable by a fresh receiver. sender={sender!.GetDiagnosticsJson()} receiver={freshReceiver!.GetDiagnosticsJson()}");
            // ПОЛЕ h264_parameter_set_prepends СНЯТО ВМЕСТЕ С УКАЗАТЕЛЕМ (#61, шаг D2b).
            //
            // Оно читалось прямо из MFT по сырому указателю. Кодировщиком теперь владеет
            // VideoStreamEncoder и пересоздаёт его при каждой перенастройке — читать его снаружи
            // значит читать освобождённую память (падение поймано замером). Само требование
            // проверяется выше и сильнее: СВЕЖИЙ приёмник декодировал принудительный ключевой
            // кадр, а это возможно только если наборы параметров в нём есть.
            //
            // Здесь остаётся проверка, что диагностика вообще описывает кодировщик.
            Assert.Contains(
                "\"encoder_backend\":",
                sender!.GetDiagnosticsJson(),
                StringComparison.Ordinal);
        }
    }

    [Fact]
    public void RealtimeMediaCapabilities_UseSnakeCaseFfiShape()
    {
        RealtimeMediaCapabilities capabilities = new(
            "dialog::acct",
            [
                new RealtimeVideoCodecCapability("h264", CallVideoRtpConstants.H264PayloadType, new RealtimeVideoCodecSide(true), new RealtimeVideoCodecSide(true), "h264/mediafoundation")
            ],
            new RealtimeVideoProfileCapability(
                1,
                1,
                1,
                1280,
                720,
                30,
                1920,
                1080,
                60,
                [CallVideoPreferences.CapturePixelFormatNv12, CallVideoPreferences.CapturePixelFormatBgra],
                CanSendVideo: true),
            (uint)CallVideoRtpConstants.MaxDatagramBytes,
            SupportsRtcpFeedback: true,
            SentAtUnixMs: 123);

        string json = JsonSerializer.Serialize(
            capabilities,
            OmniJsonSerializerContext.Default.RealtimeMediaCapabilities);

        Assert.Contains("\"dialog_key\"", json, StringComparison.Ordinal);
        Assert.Contains("\"min_width\"", json, StringComparison.Ordinal);
        Assert.Contains("\"max_datagram_bytes\"", json, StringComparison.Ordinal);
        Assert.Contains("\"pixel_formats\": [", json, StringComparison.Ordinal);
        Assert.Contains("\"supports_rtcp_feedback\": true", json, StringComparison.Ordinal);
    }

    /// Разметка провода у оболочки и у ядра (`realtime_media_wire.rs`) обязана совпадать байт в
    /// байт: свой номер вида сообщения, свои теги, свой порядок байт. Разойдутся — и на живом
    /// звонке управляющее сообщение просто исчезнет, без единой ошибки.
    ///
    /// Проверка держит ОБЕ стороны: что записанное читается обратно, и что записано оно ровно
    /// теми байтами, которые ждёт ядро, — иначе тест прошёл бы и на своей собственной,
    /// согласованной с самой собой, но неверной разметке.
    ///
    /// Раньше это же проверялось на `transport_feedback` с его тегом 0x13. Тип снят вместе с
    /// управляющим путём отчёта о доставке (отчёт едет теперь в общей очереди RTCP), и вместе с
    /// ним ушли бы ЕДИНСТВЕННЫЕ тесты этого кодека — поэтому проверка перенесена на `nack`, а не
    /// удалена.
    /// Отрицательная половина: тег ставится только тогда, когда есть что класть. Без неё
    /// проверка выше прошла бы и на кодеке, который пишет пустую запись всегда.
    [Fact]
    public void RealtimeMediaSelection_AudioOnlySessionOmitsTheVideoHalf()
    {
        // The camera-less shape. The Rust side sends no `video` key at all, and the
        // shell has to read that as a working call rather than a malformed selection.
        RealtimeMediaSelection selection = new(
            DialogKey: "dialog::acct",
            SessionId: "media-v2:dialog::acct",
            MaxDatagramBytes: CallVideoRtpConstants.MaxDatagramBytes,
            Video: null,
            SelectedAtUnixMs: 123);

        string json = JsonSerializer.Serialize(
            selection,
            OmniJsonSerializerContext.Default.RealtimeMediaSelection);
        RealtimeMediaSelection? roundTripped = JsonSerializer.Deserialize(
            "{\"dialog_key\":\"dialog::acct\",\"session_id\":\"media-v2:dialog::acct\",\"max_datagram_bytes\":1000,\"selected_at_unix_ms\":123}",
            OmniJsonSerializerContext.Default.RealtimeMediaSelection);

        Assert.DoesNotContain("\"capture_pixel_format\"", json, StringComparison.Ordinal);
        Assert.NotNull(roundTripped);
        Assert.Null(roundTripped!.Video);
        Assert.Equal("media-v2:dialog::acct", roundTripped.SessionId);
    }

    [Fact]
    public void CallVideoPreferences_ForcedCodecAdvertisesOnlySelectedCodec()
    {
        RealtimeMediaCapabilities capabilities = CallVideoPreferences.BuildCapabilities(
            "dialog::acct",
            CallVideoCodecPreference.H264,
            new CallVideoResolutionOption(1280, 720),
            CallVideoFrameRatePreference.Auto,
            TestCaptureProbe(),
            [
                new CallVideoCodecSupport(CallVideoCodecKind.H264, CallVideoRtpConstants.H264PayloadType, true, true, "h264/test"),
            ]);

        RealtimeVideoCodecCapability codec = Assert.Single(capabilities.Codecs);
        Assert.Equal("h264", codec.Codec);
        Assert.Equal(CallVideoRtpConstants.H264PayloadType, codec.PayloadType);
    }

    [Fact]
    public void CallVideoPreferences_Forced1080pAdvertisesPreferredAndMax1080p()
    {
        RealtimeMediaCapabilities capabilities = CallVideoPreferences.BuildCapabilities(
            "dialog::acct",
            CallVideoCodecPreference.Auto,
            new CallVideoResolutionOption(1920, 1080),
            CallVideoFrameRatePreference.Auto,
            TestCaptureProbe(),
            [new CallVideoCodecSupport(CallVideoCodecKind.H264, CallVideoRtpConstants.H264PayloadType, true, true, "h264/test")]);

        Assert.Equal(1920u, capabilities.Video!.PreferredWidth);
        Assert.Equal(1080u, capabilities.Video!.PreferredHeight);
        Assert.Equal(1920u, capabilities.Video!.MinWidth);
        Assert.Equal(1080u, capabilities.Video!.MinHeight);
        Assert.Equal(1920u, capabilities.Video!.MaxWidth);
        Assert.Equal(1080u, capabilities.Video!.MaxHeight);
        Assert.Contains(CallVideoPreferences.CapturePixelFormatNv12, capabilities.Video!.PixelFormats);
        Assert.Contains(CallVideoPreferences.CapturePixelFormatBgra, capabilities.Video!.PixelFormats);
    }

    [Fact]
    public void CallVideoPreferences_PrefersNv12ForNativeFriendlyCapturePath()
    {
        RealtimeMediaCapabilities capabilities = CallVideoPreferences.BuildCapabilities(
            "dialog::acct",
            CallVideoCodecPreference.Auto,
            new CallVideoResolutionOption(1280, 720),
            CallVideoFrameRatePreference.Fps30,
            TestCaptureProbe([
                new CallVideoCaptureFormat(1280, 720, 30, CallVideoPreferences.CapturePixelFormatBgra, "test"),
                new CallVideoCaptureFormat(1280, 720, 30, CallVideoPreferences.CapturePixelFormatNv12, "test"),
            ]),
            [new CallVideoCodecSupport(CallVideoCodecKind.H264, CallVideoRtpConstants.H264PayloadType, true, true, "h264/test")]);

        Assert.Collection(
            capabilities.Video!.PixelFormats,
            pixelFormat => Assert.Equal(CallVideoPreferences.CapturePixelFormatNv12, pixelFormat),
            pixelFormat => Assert.Equal(CallVideoPreferences.CapturePixelFormatBgra, pixelFormat));
    }

    [Fact]
    public void CallVideoPreferences_Forced1080p60AdvertisesFps60()
    {
        RealtimeMediaCapabilities capabilities = CallVideoPreferences.BuildCapabilities(
            "dialog::acct",
            CallVideoCodecPreference.Auto,
            new CallVideoResolutionOption(1920, 1080),
            CallVideoFrameRatePreference.Fps60,
            TestCaptureProbe(),
            [new CallVideoCodecSupport(CallVideoCodecKind.H264, CallVideoRtpConstants.H264PayloadType, true, true, "h264/test")]);

        Assert.Equal(1920u, capabilities.Video!.PreferredWidth);
        Assert.Equal(1080u, capabilities.Video!.PreferredHeight);
        Assert.Equal(60u, capabilities.Video!.MinFps);
        Assert.Equal(60u, capabilities.Video!.PreferredFps);
        Assert.Equal(60u, capabilities.Video!.MaxFps);
    }

    [Fact]
    public void CallVideoPreferences_AutoUsesProbeConfirmedProfileRange()
    {
        RealtimeMediaCapabilities capabilities = CallVideoPreferences.BuildCapabilities(
            "dialog::acct",
            CallVideoCodecPreference.Auto,
            new CallVideoResolutionOption(1280, 720),
            CallVideoFrameRatePreference.Auto,
            TestCaptureProbe([
                new CallVideoCaptureFormat(640, 480, 30, CallVideoPreferences.CapturePixelFormatBgra, "test"),
                new CallVideoCaptureFormat(1280, 720, 30, CallVideoPreferences.CapturePixelFormatNv12, "test"),
                new CallVideoCaptureFormat(1920, 1080, 60, CallVideoPreferences.CapturePixelFormatNv12, "test"),
            ]),
            [new CallVideoCodecSupport(CallVideoCodecKind.H264, CallVideoRtpConstants.H264PayloadType, true, true, "h264/test")]);

        // Auto selects the top probe-confirmed profile ≤ the 720p default
        // (commit b35dd3c reverted the Wave-6 1080p bump back to 720p to cut the CPU
        // decode/convert/blit cost until the GPU render path lands). The 1080p probe
        // format is above the cap, so Auto lands on 1280×720@30 (fps capped to
        // DefaultTargetFps).
        Assert.Equal(1280u, capabilities.Video!.PreferredWidth);
        Assert.Equal(720u, capabilities.Video!.PreferredHeight);
        Assert.Equal(30u, capabilities.Video!.PreferredFps);
        Assert.Equal(1280u, capabilities.Video!.MinWidth);
        Assert.Equal(720u, capabilities.Video!.MinHeight);
        Assert.Equal(30u, capabilities.Video!.MinFps);
        Assert.Equal(1280u, capabilities.Video!.MaxWidth);
        Assert.Equal(720u, capabilities.Video!.MaxHeight);
        Assert.Equal(30u, capabilities.Video!.MaxFps);
        string pixelFormat = Assert.Single(capabilities.Video!.PixelFormats);
        Assert.Equal(CallVideoPreferences.CapturePixelFormatNv12, pixelFormat);
    }

    [Fact]
    public void CallVideoPreferences_SlimCameraGetsItsOwnMaxAsTheDefaultInsteadOf720p()
    {
        // Раньше за это отвечал режим Auto. Auto снят вместе с авторазрешением (2026-08-17), и
        // проверка осталась той же по существу: камера, которая 720p не умеет, не должна получить
        // 720p «по умолчанию» и превратиться в decode-only с погашенной кнопкой.
        CallVideoResolutionOption defaultForSlimCamera = CallVideoPreferences.DefaultResolution(
            CallVideoPreferences.BuildResolutionOptions(424, 240));
        Assert.Equal(new CallVideoResolutionOption(424, 240), defaultForSlimCamera);

        RealtimeMediaCapabilities capabilities = CallVideoPreferences.BuildCapabilities(
            "dialog::acct",
            CallVideoCodecPreference.Auto,
            defaultForSlimCamera,
            CallVideoFrameRatePreference.Auto,
            TestCaptureProbe([
                new CallVideoCaptureFormat(320, 180, 30, CallVideoPreferences.CapturePixelFormatNv12, "test"),
                new CallVideoCaptureFormat(424, 240, 30, CallVideoPreferences.CapturePixelFormatNv12, "test"),
            ]),
            [new CallVideoCodecSupport(CallVideoCodecKind.H264, CallVideoRtpConstants.H264PayloadType, true, true, "h264/test")]);

        Assert.True(capabilities.Video!.CanSendVideo);
        Assert.Equal(424u, capabilities.Video!.PreferredWidth);
        Assert.Equal(240u, capabilities.Video!.PreferredHeight);
        Assert.Equal(424u, capabilities.Video!.MaxWidth);
        Assert.Equal(240u, capabilities.Video!.MaxHeight);
    }

    [Fact]
    public void CallVideoPreferences_ForcedProfileTheCameraCannotServeBecomesDecodeOnly()
    {
        // 1080p60 requested from a 720p30 camera. We cannot SEND that, but we can still
        // decode it, so we publish the profile marked decode-only instead of refusing to
        // publish at all — refusing used to take the call's audio down with the video.
        CallVideoCaptureProbeResult probe = TestCaptureProbe([
            new CallVideoCaptureFormat(1280, 720, 30, CallVideoPreferences.CapturePixelFormatBgra, "test"),
        ]);

        RealtimeMediaCapabilities capabilities = CallVideoPreferences.BuildCapabilities(
            "dialog::acct",
            CallVideoCodecPreference.Auto,
            new CallVideoResolutionOption(1920, 1080),
            CallVideoFrameRatePreference.Fps60,
            probe,
            [new CallVideoCodecSupport(CallVideoCodecKind.H264, CallVideoRtpConstants.H264PayloadType, true, true, "h264/test")]);

        Assert.NotNull(capabilities.Video);
        Assert.False(capabilities.Video!.CanSendVideo);
        // And the picker still refuses the profile where the user can see why.
        Assert.False(CallVideoPreferences.CanCaptureVideo(
            new CallVideoResolutionOption(1920, 1080),
            CallVideoFrameRatePreference.Fps60,
            probe));
    }

    [Fact]
    public void CallVideoPreferences_NoCameraStillAnnouncesUsAsDecodeOnly()
    {
        // The live failure: this machine published NOTHING, so no session was ever
        // negotiated and neither peer started audio. It must announce itself.
        RealtimeMediaCapabilities capabilities = CallVideoPreferences.BuildCapabilities(
            "dialog::acct",
            CallVideoCodecPreference.Auto,
            new CallVideoResolutionOption(1280, 720),
            CallVideoFrameRatePreference.Auto,
            TestCaptureProbe([]),
            [new CallVideoCodecSupport(CallVideoCodecKind.H264, CallVideoRtpConstants.H264PayloadType, true, true, "h264/test")]);

        Assert.NotNull(capabilities.Video);
        Assert.False(capabilities.Video!.CanSendVideo);
        Assert.NotEmpty(capabilities.Codecs);
    }

    /// <summary>
    /// No camera, but the machine can share its screen: that is a video source, so it announces
    /// itself as able to SEND — otherwise two camera-less machines negotiate audio-only and can
    /// never show each other a screen. The negative half is the test above: without screen capture
    /// the same machine stays decode-only.
    /// </summary>
    [Fact]
    public void CallVideoPreferences_NoCameraButScreenShareCanSendVideo()
    {
        RealtimeMediaCapabilities capabilities = CallVideoPreferences.BuildCapabilities(
            "dialog::acct",
            CallVideoCodecPreference.Auto,
            new CallVideoResolutionOption(1280, 720),
            CallVideoFrameRatePreference.Auto,
            TestCaptureProbe([]),
            [new CallVideoCodecSupport(CallVideoCodecKind.H264, CallVideoRtpConstants.H264PayloadType, true, true, "h264/test")],
            canShareScreen: true);

        Assert.NotNull(capabilities.Video);
        Assert.True(capabilities.Video!.CanSendVideo);
        // The camera button still says no — it is about the camera, not the screen.
        Assert.False(CallVideoPreferences.CanCaptureVideo(
            new CallVideoResolutionOption(1280, 720),
            CallVideoFrameRatePreference.Auto,
            TestCaptureProbe([])));
    }

    [Fact]
    public void CallVideoPreferences_AdvertisesOnlyProbeConfirmedPixelFormatsForForcedProfile()
    {
        RealtimeMediaCapabilities capabilities = CallVideoPreferences.BuildCapabilities(
            "dialog::acct",
            CallVideoCodecPreference.Auto,
            new CallVideoResolutionOption(1280, 720),
            CallVideoFrameRatePreference.Fps30,
            TestCaptureProbe([
                new CallVideoCaptureFormat(1280, 720, 30, CallVideoPreferences.CapturePixelFormatNv12, "test"),
                new CallVideoCaptureFormat(640, 480, 30, CallVideoPreferences.CapturePixelFormatBgra, "test"),
            ]),
            [new CallVideoCodecSupport(CallVideoCodecKind.H264, CallVideoRtpConstants.H264PayloadType, true, true, "h264/test")]);

        string pixelFormat = Assert.Single(capabilities.Video!.PixelFormats);
        Assert.Equal(CallVideoPreferences.CapturePixelFormatNv12, pixelFormat);
        Assert.Equal(1280u, capabilities.Video!.MinWidth);
        Assert.Equal(720u, capabilities.Video!.MinHeight);
        Assert.Equal(30u, capabilities.Video!.MinFps);
    }

    [Fact]
    public void CallVideoPreferences_NoUsableCodecAnnouncesNoVideoAtAll()
    {
        // Forcing H.264 when no hardware-eligible H.264 codec is advertised leaves us
        // with nothing to offer the video negotiation. We say exactly that — an empty
        // codec list and no video profile — rather than declining to speak at all, which
        // is what left calls silent. The peer's selection then reports "no common
        // realtime video codec" and runs the call audio-only.
        RealtimeMediaCapabilities capabilities = CallVideoPreferences.BuildCapabilities(
            "dialog::acct",
            CallVideoCodecPreference.H264,
            new CallVideoResolutionOption(1280, 720),
            CallVideoFrameRatePreference.Auto,
            TestCaptureProbe([
                new CallVideoCaptureFormat(1280, 720, 30, CallVideoPreferences.CapturePixelFormatNv12, "test"),
            ]),
            supportedCodecs: []);

        Assert.Empty(capabilities.Codecs);
        Assert.Null(capabilities.Video);
    }

    [Fact]
    public void CallVideoPreferences_SoftwareOnlyH264IsNeverAdvertised()
    {
        // Forced software H.264 stays forbidden (docs/decisions/h264-runtime-proof.md):
        // it is simply not advertised, so no peer can negotiate it.
        RealtimeMediaCapabilities capabilities = CallVideoPreferences.BuildCapabilities(
            "dialog::acct",
            CallVideoCodecPreference.H264,
            new CallVideoResolutionOption(1280, 720),
            CallVideoFrameRatePreference.Auto,
            TestCaptureProbe(),
            [
                new CallVideoCodecSupport(CallVideoCodecKind.H264, CallVideoRtpConstants.H264PayloadType, false, false, "native/libwebrtc h264 software"),
            ]);

        Assert.Empty(capabilities.Codecs);
        Assert.Null(capabilities.Video);
    }

    [Fact]
    public void CallVideoPreferences_CodecImplementationFitsProtocolLimit()
    {
        string longImplementation = "native/libwebrtc h264; loaded=D:\\very\\long\\path\\omni_webrtc_media.dll; " +
            new string('x', 256);

        RealtimeMediaCapabilities capabilities = CallVideoPreferences.BuildCapabilities(
            "dialog::acct",
            CallVideoCodecPreference.Auto,
            new CallVideoResolutionOption(1280, 720),
            CallVideoFrameRatePreference.Auto,
            TestCaptureProbe(),
            [new CallVideoCodecSupport(CallVideoCodecKind.H264, CallVideoRtpConstants.H264PayloadType, true, true, longImplementation)]);

        RealtimeVideoCodecCapability codec = Assert.Single(capabilities.Codecs);
        Assert.NotNull(codec.Implementation);
        Assert.InRange(System.Text.Encoding.UTF8.GetByteCount(codec.Implementation), 1, 128);
    }

    [Fact]
    public void CallVideoHealthClassifier_LowCameraFpsIsCaptureThrottledNotNetworkLoss()
    {
        string health = CallVideoHealthClassifier.Classify(new CallVideoHealthSample(
            CaptureDiagnostics: "1280x720@30 WinRT/NV12 throttle=source",
            RemoteLossFraction: 0,
            DecodeStalls: 0,
            RenderStalls: 0,
            CaptureLoopFps: 10,
            TargetFps: 30));

        Assert.Equal("capture_throttled", health);
    }

    private static CallVideoCaptureProbeResult TestCaptureProbe(
        IEnumerable<CallVideoCaptureFormat>? formats = null)
        => CallVideoCaptureProbe.FromObservedFormats(
            formats ??
            [
                new CallVideoCaptureFormat(854, 480, 30, CallVideoPreferences.CapturePixelFormatBgra, "test"),
                new CallVideoCaptureFormat(1280, 720, 30, CallVideoPreferences.CapturePixelFormatBgra, "test"),
                new CallVideoCaptureFormat(1920, 1080, 60, CallVideoPreferences.CapturePixelFormatBgra, "test"),
                new CallVideoCaptureFormat(1920, 1080, 60, CallVideoPreferences.CapturePixelFormatNv12, "test"),
            ]);

    private static NativeWebRtcMediaAbrSample CreateStableAbrSample(long nowMs) => new(nowMs);

    /// Push frames until the session has actually handed a packet to its sink.
    private static void SendOneFrame(NativeWebRtcMediaEngine.Session session, int width, int height)
    {
        CreateI420ColorBars(width, height, out byte[] yPlane, out byte[] uPlane, out byte[] vPlane);
        void Push(long timestampUs) => session.PushI420Frame(
            yPlane, width, uPlane, (width + 1) / 2, vPlane, (width + 1) / 2, width, height, timestampUs,
            forceKeyFrame: true);
        Push(4_000_000);
        NativeWebRtcRtpPacketBatch batch = DrainRtpAllowingEncoderLatency(session, Push);
        Assert.True(batch.PacketCount > 0, "the session sent nothing");
    }

    /// Слить RTP, учитывая конвейерную задержку АППАРАТНОГО кодировщика.
    ///
    /// Тесты писались с допущением «кадр подали — пакет обязан быть». Оно верно для программного
    /// кодировщика и неверно для аппаратного: у того своя задержка. Замер 2026-08-15 на этой
    /// машине: 1 кадр -> 0 пакетов, 5 кадров -> 9.
    ///
    /// Раньше это не всплывало, потому что аппаратная сессия не поднималась вовсе (две ошибки
    /// контракта Media Foundation, см. docs/bugs/hardware-h264-never-started.md), и все эти тесты
    /// молча выходили через `ShouldRunNativeSessionTests()`. С рабочим кодировщиком они впервые
    /// начали исполняться — и допущение стало видно.
    ///
    /// Досылать нужно ТОТ ЖЕ кадр: часть тестов проверяет сохранение цвета, и подмена содержимого
    /// проверяла бы не то, что отправляли.
    /// <summary>
    /// Дождаться пакетов, НЕ производя лишних кадров.
    /// </summary>
    /// <remarks>
    /// Здесь стояло «пусто — толкнём ещё кадр», без единой паузы, и до перехода на апстримовый
    /// отправитель это было верно: задержка была у кодировщика, и подтолкнуть его новым кадром
    /// действительно помогало. Теперь задержка у ПЕЙСЕРА — он тикает сам, раз в 5 мс, — и лечить
    /// её лишними кадрами значит производить то, чего тест не заказывал: в выборку попадали
    /// пакеты двух-трёх кадров, и приёмник честно собирал больше кадров, чем тест ожидал.
    ///
    /// Поэтому сначала ЖДЁМ, и только если ожидание ничего не дало — подталкиваем кадром. Обе
    /// причины задержки остаются покрытыми, а лишних кадров не появляется.
    /// </remarks>
    private static NativeWebRtcRtpPacketBatch DrainRtpAllowingEncoderLatency(
        NativeWebRtcMediaEngine.Session sender,
        Action<long> pushSameFrameAgain,
        uint maxPackets = 128)
    {
        NativeWebRtcRtpPacketBatch batch = sender.DrainRtpPackets(maxPackets);
        for (int wait = 0; wait < 100 && batch.PacketCount == 0; wait++)
        {
            Thread.Sleep(10);
            batch = sender.DrainRtpPackets(maxPackets);
        }

        long timestampUs = 5_000_000;
        for (int attempt = 0; attempt < 5 && batch.PacketCount == 0; attempt++)
        {
            pushSameFrameAgain(timestampUs);
            timestampUs += 33_000;
            for (int wait = 0; wait < 20 && batch.PacketCount == 0; wait++)
            {
                Thread.Sleep(10);
                batch = sender.DrainRtpPackets(maxPackets);
            }
        }

        return batch;
    }

    /// <summary>Прочитать целое из диагностического JSON нативной сессии.</summary>
    private static long ReadDiagnosticsInt(string diagnostics, string key)
    {
        int at = diagnostics.IndexOf(key, StringComparison.Ordinal);
        if (at < 0)
        {
            return -1;
        }

        int from = at + key.Length;
        int to = from;
        while (to < diagnostics.Length && (char.IsDigit(diagnostics[to]) || diagnostics[to] == '-'))
        {
            to++;
        }

        return to > from && long.TryParse(diagnostics[from..to], out long value) ? value : -1;
    }

    private static bool ShouldRunNativeSessionTests()
    {
        NativeWebRtcMediaProbeResult probe = CallVideoCodecFactory.GetNativeWebRtcProbeResult();
        if (probe.NativeSessionScaffold)
        {
            return true;
        }

        Assert.True(
            string.IsNullOrWhiteSpace(Environment.GetEnvironmentVariable("OMNI_WEBRTC_MEDIA_DLL")),
            $"OMNI_WEBRTC_MEDIA_DLL was set, but native session scaffold is unavailable: {probe.Summary}");
        return false;
    }

    private static bool WaitForNativeI420Frame(
        NativeWebRtcMediaEngine.Session receiver,
        out NativeWebRtcDecodedI420Frame frame)
    {
        for (int attempt = 0; attempt < 50; attempt++)
        {
            if (receiver.TryDrainI420Frame(out frame))
            {
                return true;
            }

            Thread.Sleep(10);
        }

        frame = default;
        return false;
    }

    private static bool WaitForNativeBgraFrame(
        NativeWebRtcMediaEngine.Session receiver,
        out NativeWebRtcDecodedBgraFrame frame)
    {
        for (int attempt = 0; attempt < 50; attempt++)
        {
            if (receiver.TryDrainBgraFrame(out frame))
            {
                return true;
            }

            Thread.Sleep(10);
        }

        frame = default;
        return false;
    }

    private static NativeWebRtcRtpPacketBatch PushNv12FrameAndDrainRtp(
        NativeWebRtcMediaEngine.Session sender,
        byte[] yPlane,
        byte[] uvPlane,
        long timestampUs,
        bool forceKeyFrame)
    {
        // ВОСЕМЬ ПОПЫТОК БЕЗ ПАУЗ БОЛЬШЕ НЕ ХВАТАЕТ (#61, шаг D2b).
        //
        // Кадр кодировался ВНУТРИ push, и пакеты можно было выгрести сразу. Теперь кадр уходит в
        // VideoStreamEncoder, тот кодирует на своей очереди, а пейсер отпускает пакеты по часам.
        // Между подачей и появлением пакетов надо ДАТЬ ВРЕМЯ — иначе тест проверяет не путь, а
        // скорость реакции машины.
        for (int attempt = 0; attempt < 60; attempt++)
        {
            sender.PushNv12Frame(
                yPlane,
                320,
                uvPlane,
                320,
                320,
                180,
                timestampUs + (attempt * 33_333),
                forceKeyFrame);
            NativeWebRtcRtpPacketBatch batch = sender.DrainRtpPackets(maxPackets: 128);
            if (batch.PacketCount > 0)
            {
                return batch;
            }

            Thread.Sleep(10);
        }

        return new NativeWebRtcRtpPacketBatch([], 0, 0);
    }

    private static IReadOnlyList<byte[]> SplitNativeRtpBatch(NativeWebRtcRtpPacketBatch batch)
    {
        int offset = 0;
        ReadOnlySpan<byte> buffer = batch.Buffer.AsSpan(0, batch.Length);
        List<byte[]> packets = [];
        for (uint i = 0; i < batch.PacketCount; i++)
        {
            Assert.True(buffer.Length - offset >= sizeof(ushort));
            int packetLength = BinaryPrimitives.ReadUInt16LittleEndian(buffer.Slice(offset, sizeof(ushort)));
            offset += sizeof(ushort);
            Assert.True(packetLength > 0);
            Assert.True(buffer.Length - offset >= packetLength);
            packets.Add(buffer.Slice(offset, packetLength).ToArray());
            offset += packetLength;
        }

        Assert.Equal(buffer.Length, offset);
        return packets;
    }

    private static byte[] PackSingleNativeRtpPacket(byte[] packet)
    {
        byte[] batch = new byte[sizeof(ushort) + packet.Length];
        BinaryPrimitives.WriteUInt16LittleEndian(batch.AsSpan(0, sizeof(ushort)), checked((ushort)packet.Length));
        packet.CopyTo(batch.AsSpan(sizeof(ushort)));
        return batch;
    }

    /// The first MEDIA packet of a native batch (LE16 length, then the packet). Since ABI 17 the
    /// sender also puts its own RTCP (sender reports) into the same pipe, so RTCP (second byte
    /// 192..223, RFC 5761) is skipped.
    private static ReadOnlySpan<byte> FirstNativeRtpPacket(ReadOnlySpan<byte> nativeBatch)
    {
        int offset = 0;
        while (offset + 2 <= nativeBatch.Length)
        {
            ushort packetLength = BitConverter.ToUInt16(nativeBatch.Slice(offset, 2));
            Assert.True(nativeBatch.Length >= offset + 2 + packetLength);
            ReadOnlySpan<byte> packet = nativeBatch.Slice(offset + 2, packetLength);
            if (packet.Length >= 12 && (packet[1] < 192 || packet[1] > 223))
            {
                return packet;
            }

            offset += 2 + packetLength;
        }

        Assert.Fail("the batch holds no RTP packet");
        return default;
    }

    private static ushort ReadFirstNativeRtpSequence(ReadOnlySpan<byte> nativeBatch)
    {
        ReadOnlySpan<byte> packet = FirstNativeRtpPacket(nativeBatch);
        return (ushort)((packet[2] << 8) | packet[3]);
    }

    private static uint ReadFirstNativeRtpSsrc(ReadOnlySpan<byte> nativeBatch)
        => System.Buffers.Binary.BinaryPrimitives.ReadUInt32BigEndian(FirstNativeRtpPacket(nativeBatch).Slice(8, 4));

    /// One generic NACK (RFC 4585 §6.2.1) for one sequence number, framed as a one-packet batch
    /// (LE16 length, then the packet) — the shape the session takes inbound packets in.
    private static byte[] BuildGenericNackBatch(uint mediaSsrc, ushort sequence)
    {
        byte[] batch = new byte[2 + 16];
        batch[0] = 16;
        Span<byte> nack = batch.AsSpan(2);
        nack[0] = 0x81;
        nack[1] = 205;
        nack[3] = 3;
        System.Buffers.Binary.BinaryPrimitives.WriteUInt32BigEndian(nack[4..], 0x12345678u);
        System.Buffers.Binary.BinaryPrimitives.WriteUInt32BigEndian(nack[8..], mediaSsrc);
        System.Buffers.Binary.BinaryPrimitives.WriteUInt16BigEndian(nack[12..], sequence);
        return batch;
    }

    private static void CreateI420ColorBars(
        int width,
        int height,
        out byte[] yPlane,
        out byte[] uPlane,
        out byte[] vPlane)
    {
        yPlane = new byte[width * height];
        int chromaWidth = (width + 1) / 2;
        int chromaHeight = (height + 1) / 2;
        uPlane = new byte[chromaWidth * chromaHeight];
        vPlane = new byte[chromaWidth * chromaHeight];

        for (int y = 0; y < height; y++)
        {
            int rowOffset = y * width;
            for (int x = 0; x < width; x++)
            {
                yPlane[rowOffset + x] = x < width / 3 ? (byte)76 : x < width * 2 / 3 ? (byte)149 : (byte)29;
            }
        }

        for (int y = 0; y < chromaHeight; y++)
        {
            int rowOffset = y * chromaWidth;
            for (int x = 0; x < chromaWidth; x++)
            {
                if (x < chromaWidth / 3)
                {
                    uPlane[rowOffset + x] = 84;
                    vPlane[rowOffset + x] = byte.MaxValue;
                }
                else if (x < chromaWidth * 2 / 3)
                {
                    uPlane[rowOffset + x] = 43;
                    vPlane[rowOffset + x] = 21;
                }
                else
                {
                    uPlane[rowOffset + x] = byte.MaxValue;
                    vPlane[rowOffset + x] = 107;
                }
            }
        }
    }

    private static void CreateNv12ColorBars(
        int width,
        int height,
        out byte[] yPlane,
        out byte[] uvPlane)
    {
        CreateI420ColorBars(width, height, out yPlane, out byte[] uPlane, out byte[] vPlane);
        int chromaWidth = (width + 1) / 2;
        int chromaHeight = (height + 1) / 2;
        uvPlane = new byte[width * chromaHeight];
        for (int y = 0; y < chromaHeight; y++)
        {
            int planarOffset = y * chromaWidth;
            int uvOffset = y * width;
            for (int x = 0; x < chromaWidth; x++)
            {
                uvPlane[uvOffset + (x * 2)] = uPlane[planarOffset + x];
                uvPlane[uvOffset + (x * 2) + 1] = vPlane[planarOffset + x];
            }
        }
    }

    private static bool DecodedI420ColorBarsArePlausible(byte[] frame, int width, int height)
    {
        if (frame.Length < width * height ||
            width < 96 ||
            height < 64)
        {
            return false;
        }

        double left = AverageLuma(frame, width, height, width / 12, width / 4);
        double middle = AverageLuma(frame, width, height, width * 5 / 12, width * 7 / 12);
        double right = AverageLuma(frame, width, height, width * 3 / 4, width * 11 / 12);
        return middle > left + 20.0 && left > right + 20.0;
    }

    private static double AverageLuma(byte[] frame, int width, int height, int startX, int endX)
    {
        long sum = 0;
        long count = 0;
        for (int y = 0; y < height; y++)
        {
            int rowOffset = y * width;
            for (int x = startX; x < endX; x++)
            {
                sum += frame[rowOffset + x];
                count++;
            }
        }

        return count == 0 ? 0 : (double)sum / count;
    }

    private static byte[] CreateBgraColorBarsFrame(int width, int height)
    {
        byte[] frame = new byte[width * height * 4];
        for (int y = 0; y < height; y++)
        {
            for (int x = 0; x < width; x++)
            {
                int offset = (y * width + x) * 4;
                if (x < width / 3)
                {
                    frame[offset] = 0;
                    frame[offset + 1] = 0;
                    frame[offset + 2] = byte.MaxValue;
                }
                else if (x < width * 2 / 3)
                {
                    frame[offset] = 0;
                    frame[offset + 1] = byte.MaxValue;
                    frame[offset + 2] = 0;
                }
                else
                {
                    frame[offset] = byte.MaxValue;
                    frame[offset + 1] = 0;
                    frame[offset + 2] = 0;
                }

                frame[offset + 3] = byte.MaxValue;
            }
        }

        return frame;
    }

    private static void AssertDominantColor(
        byte[] bgra,
        int width,
        int height,
        int centerX,
        int centerY,
        DominantColor expected)
    {
        (long red, long green, long blue) = AverageBgra(bgra, width, height, centerX, centerY);
        long expectedValue = expected switch
        {
            DominantColor.Red => red,
            DominantColor.Green => green,
            DominantColor.Blue => blue,
            _ => throw new ArgumentOutOfRangeException(nameof(expected), expected, null),
        };
        long nextValue = expected switch
        {
            DominantColor.Red => Math.Max(green, blue),
            DominantColor.Green => Math.Max(red, blue),
            DominantColor.Blue => Math.Max(red, green),
            _ => throw new ArgumentOutOfRangeException(nameof(expected), expected, null),
        };
        Assert.True(
            expectedValue > 96 && expectedValue - nextValue > 30,
            $"Expected {expected} dominance near ({centerX},{centerY}), got R={red} G={green} B={blue}.");
    }

    private static (long Red, long Green, long Blue) AverageBgra(byte[] bgra, int width, int height, int centerX, int centerY)
    {
        const int radius = 6;
        long red = 0;
        long green = 0;
        long blue = 0;
        long count = 0;
        int minX = Math.Max(0, centerX - radius);
        int maxX = Math.Min(width - 1, centerX + radius);
        int minY = Math.Max(0, centerY - radius);
        int maxY = Math.Min(height - 1, centerY + radius);
        for (int y = minY; y <= maxY; y++)
        {
            for (int x = minX; x <= maxX; x++)
            {
                int offset = (y * width + x) * 4;
                blue += bgra[offset];
                green += bgra[offset + 1];
                red += bgra[offset + 2];
                count++;
            }
        }

        return (red / count, green / count, blue / count);
    }

    // Решение ABR о разрешении должно доезжать и до демонстрации экрана.
    //
    // У камеры оно исполняется перезапуском захвата под ограничением профиля; у экрана
    // перезапускать нечего, поэтому кадр масштабируется. До 2026-08-14 этого шага не было: ABR
    // сбавлял битрейт и fps, а экран продолжал кодироваться целиком — и ключевой кадр оставался
    // неподъёмным именно в том режиме, где у пользователя умирало видео
    // (docs/bugs/video-dies-on-packet-loss.md).
    //
    // Проверяется контракт выбора размера, на котором эта правка стоит: вписать в рамку с
    // сохранением пропорций, чётные стороны, и НЕ трогать кадр, который и так не больше цели.
    [Fact]
    public void ScreenFrameScaling_FitsTheAbrTargetWithoutStretching()
    {
        // 1920x1080 при цели 640x360 — пропорции совпадают, берётся цель как есть.
        Assert.True(CallVideoFrameScaler.TrySelectScaledSize(1920, 1080, 640, 360, out int w, out int h));
        Assert.Equal(640, w);
        Assert.Equal(360, h);

        // Экран 16:10 при цели 16:9 — вписываем, а не растягиваем: пропорции исходника сохранены.
        Assert.True(CallVideoFrameScaler.TrySelectScaledSize(1920, 1200, 640, 360, out w, out h));
        Assert.True(w <= 640 && h <= 360, $"вышли за рамку: {w}x{h}");
        Assert.Equal(0, w % 2);
        Assert.Equal(0, h % 2);
        double sourceAspect = 1920d / 1200d;
        Assert.True(
            Math.Abs((w / (double)h) - sourceAspect) / sourceAspect < 0.03,
            $"пропорции искажены: {w}x{h} против исходных 1920x1200");

        // Кадр уже не больше цели — масштабировать нечего, кадр не трогаем.
        Assert.False(CallVideoFrameScaler.TrySelectScaledSize(640, 360, 1280, 720, out _, out _));
        Assert.False(CallVideoFrameScaler.TrySelectScaledSize(640, 360, 640, 360, out _, out _));
    }

    // ИСТОЧНИК БЕЗ ОБЪЯВЛЕННОЙ ЧАСТОТЫ ОБЯЗАН ПРОРЕЖИВАТЬСЯ.
    //
    // Демонстрация экрана частоты не объявляет: кадр приходит, когда на экране что-то изменилось.
    // До 2026-08-27 такой источник не прореживался ВООБЩЕ — неизвестная частота подменялась
    // согласованной, условие «хотим не меньше, чем даёт источник» срабатывало на равенстве, и в
    // кодировщик уходил каждый кадр. Живой замер того дня: профиль 30 к/с, `sentFps` на
    // демонстрации 33-38.
    [Fact]
    public void EventDrivenSource_IsThrottledToTheTargetRate()
    {
        long lastSelectedAtMs = 0;
        int selected = 0;
        // Монитор 60 Гц: кадр каждые ~16.7 мс в течение секунды.
        for (int i = 0; i < 60; i++)
        {
            long nowMs = (long)Math.Round(i * 1000.0 / 60.0);
            if (CallVideoFrameSelector.SelectFromEventDrivenSource(30, nowMs, ref lastSelectedAtMs))
            {
                selected++;
            }
        }

        Assert.InRange(selected, 29, 31);
    }

    // ...И РОВНО ПОЭТОМУ У ДЕДЛАЙНА ЕСТЬ ДОПУСК.
    //
    // Источник событий, идущий сам примерно с целевой частотой (статичный экран обновляется 30 раз
    // в секунду при цели 30), без допуска терял бы каждый второй кадр на дрожании в пару
    // миллисекунд и давал 15 к/с вместо 30. Это та же ловушка квантования, из-за которой камеру
    // прореживают счётом, а не дедлайном (docs/bugs/media-hitching.md).
    [Fact]
    public void EventDrivenSource_AtTheTargetRate_KeepsEveryFrame()
    {
        long lastSelectedAtMs = 0;
        int selected = 0;
        for (int i = 0; i < 30; i++)
        {
            // 33.3 мс шаг с дрожанием +-1 мс.
            long nowMs = (long)Math.Round(i * 1000.0 / 30.0) + (i % 2 == 0 ? -1 : 1);
            if (CallVideoFrameSelector.SelectFromEventDrivenSource(30, nowMs, ref lastSelectedAtMs))
            {
                selected++;
            }
        }

        Assert.Equal(30, selected);
    }

    // Источник с объявленной частотой прореживается СЧЁТОМ, и это поведение правка не тронула.
    [Fact]
    public void DeclaredRateSource_TakesExactlyTheWantedShare()
    {
        long credit = 0;
        int selected = 0;
        for (int i = 0; i < 30; i++)
        {
            if (CallVideoFrameSelector.SelectFromDeclaredRate(24, 30, ref credit))
            {
                selected++;
            }
        }

        Assert.Equal(24, selected);

        // Цель не ниже источника — берём всё, ничего не накапливая.
        long fullCredit = 0;
        for (int i = 0; i < 10; i++)
        {
            Assert.True(CallVideoFrameSelector.SelectFromDeclaredRate(30, 30, ref fullCredit));
        }
    }

    private enum DominantColor
    {
        Red,
        Green,
        Blue,
    }

    [Fact]
    public void CallVideoPreferences_ResolutionOptionsComeFromTheSourceAndDefaultTo720p()
    {
        IReadOnlyList<CallVideoResolutionOption> options =
            CallVideoPreferences.BuildResolutionOptions(1920, 1080);

        // Верхняя строка — максимум источника, ничего выше него не предлагаем.
        Assert.Equal(new CallVideoResolutionOption(1920, 1080), options[0]);
        Assert.All(options, option => Assert.True(option.Height <= 1080));
        Assert.Contains(new CallVideoResolutionOption(1280, 720), options);

        Assert.Equal(
            new CallVideoResolutionOption(1280, 720),
            CallVideoPreferences.DefaultResolution(options));
    }

    [Fact]
    public void CallVideoPreferences_DefaultFallsBackToTheSourceMaxWhenItIsBelow720p()
    {
        // Камера отдаёт максимум 640x480 — «базово 720p» тут неисполнимо, и по умолчанию
        // предлагаем её собственный максимум, а не ступень, которой у неё нет.
        IReadOnlyList<CallVideoResolutionOption> options =
            CallVideoPreferences.BuildResolutionOptions(640, 480);

        Assert.Equal(new CallVideoResolutionOption(640, 480), options[0]);
        Assert.DoesNotContain(options, option => option.Height > 480);
        Assert.Equal(
            new CallVideoResolutionOption(640, 480),
            CallVideoPreferences.DefaultResolution(options));
    }

    [Fact]
    public void CallVideoPreferences_ResolutionOptionsKeepBothSidesEven()
    {
        // 1366x768 — обычная матрица ноутбука, и 1366*540/768 = 960.6. Нечётная сторона роняет
        // создание сессии H.264, поэтому округление обязано быть до чётного.
        IReadOnlyList<CallVideoResolutionOption> options =
            CallVideoPreferences.BuildResolutionOptions(1366, 768);

        Assert.All(options, option =>
        {
            Assert.Equal(0, option.Width % 2);
            Assert.Equal(0, option.Height % 2);
        });
    }

    [Fact]
    public void CallVideoPreferences_NoSourceMeansNoChoiceButStillANonZeroDefault()
    {
        IReadOnlyList<CallVideoResolutionOption> options =
            CallVideoPreferences.BuildResolutionOptions(0, 0);

        Assert.Empty(options);
        CallVideoResolutionOption fallback = CallVideoPreferences.DefaultResolution(options);
        Assert.True(fallback.Width > 0);
        Assert.True(fallback.Height > 0);
    }
}
