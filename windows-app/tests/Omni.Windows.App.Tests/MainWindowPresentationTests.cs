using Microsoft.UI.Xaml;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Chats;
using Omni.Windows.App.Presentation;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class MainWindowPresentationTests
{
    [Fact]
    public void A_File_That_Is_Simply_Here_Says_Nothing()
    {
        static AttachmentRecord File(string state, bool serving) => new(
            MessageId: "m", FileName: "f.bin", FileSizeBytes: 10, MimeType: null, ContentIdHex: "abc",
            ContentEncryption: "chacha20_poly1305_v1", State: state, BytesTransferred: 10,
            LocalBlobAvailable: true, LocalServingEnabled: serving, RemoteHolderAvailable: true,
            LocalSavedPath: "C:\f.bin", LastError: null);
        // A file on this disk is the normal case: the bubble stays quiet (owner, 2026-10-13: «Сохранено
        // локально» stood under every file).
        Assert.Equal(string.Empty, MainWindowPresentation.BuildGroupAttachmentStatusText(File("downloaded", true), false));
        Assert.Equal(MainWindowPresentation.BuildAttachmentStatusText(File("downloaded", false), false),
            MainWindowPresentation.BuildGroupAttachmentStatusText(File("downloaded", false), false));
        Assert.Equal(MainWindowPresentation.BuildAttachmentStatusText(File("available", true), true),
            MainWindowPresentation.BuildGroupAttachmentStatusText(File("available", true), true));
        // A downloaded copy served on is still just here, and not the downloader's to revoke.
        Assert.Equal(string.Empty, MainWindowPresentation.BuildAttachmentStatusText(File("downloaded", true), false));
        Assert.False(MainWindowPresentation.CanRevokeAttachmentAccess(File("downloaded", true)));
        Assert.True(MainWindowPresentation.CanRevokeAttachmentAccess(File("available", true)));
    }

    [Fact]
    public void A_File_Being_Prepared_Shows_How_Far_And_Can_Only_Be_Cancelled()
    {
        // Our big file, its tree still being read: the source is on this disk, but the core does not
        // count it as a copy to serve yet (local_blob_available=false).
        static AttachmentRecord Ours(string state, long read, string? error = null) => new(
            MessageId: "m", FileName: "big.iso", FileSizeBytes: 400, MimeType: null, ContentIdHex: "",
            ContentEncryption: "e2ee_media", State: state, BytesTransferred: read,
            LocalBlobAvailable: false, LocalServingEnabled: true, RemoteHolderAvailable: false,
            LocalSavedPath: @"C:ig.iso", LastError: error);
        Assert.Equal("Подготовка 25%", MainWindowPresentation.BuildAttachmentStatusText(Ours("preparing", 100), true));
        Assert.True(MainWindowPresentation.ShouldShowAttachmentProgress(Ours("preparing", 100)));
        Assert.Equal(25d, MainWindowPresentation.BuildAttachmentProgressValue(Ours("preparing", 100)));
        Assert.True(MainWindowPresentation.IsAttachmentPreparing(Ours("preparing", 100)));
        Assert.False(MainWindowPresentation.CanDownloadAttachment(Ours("preparing", 100)));
        Assert.False(MainWindowPresentation.CanRevokeAttachmentAccess(Ours("preparing", 100)));
        Assert.Equal("Отправка отменена", MainWindowPresentation.BuildAttachmentStatusText(Ours("failed", 0, "cancelled"), true));
        Assert.Equal("Не удалось передать", MainWindowPresentation.BuildAttachmentStatusText(Ours("failed", 0, "gone"), true));
    }

    [Fact]
    public void A_Group_File_Coming_From_Several_Members_Says_How_Many()
    {
        static AttachmentRecord Downloading(int sources) => new(
            MessageId: "m", FileName: "f.bin", FileSizeBytes: 400, MimeType: null, ContentIdHex: "abc",
            ContentEncryption: "e2ee_media", State: "downloading", BytesTransferred: 100,
            LocalBlobAvailable: false, LocalServingEnabled: false, RemoteHolderAvailable: true,
            LocalSavedPath: null, LastError: null, SourceCount: sources);
        Assert.Equal("Загрузка 25% · источников: 3", MainWindowPresentation.BuildGroupAttachmentStatusText(Downloading(3), false));
        // One source, or not counted: just the progress.
        Assert.Equal("Загрузка 25%", MainWindowPresentation.BuildGroupAttachmentStatusText(Downloading(1), false));
        Assert.Equal("Загрузка 25%", MainWindowPresentation.BuildAttachmentStatusText(Downloading(0), false));
    }

    [Fact]
    public void MessageDeliveryPresentation_MapsCurrentStatusesToStableVisualStates()
    {
        Assert.Equal(MessageDeliveryVisualState.Pending, MainWindowPresentation.MapMessageDeliveryVisualState("queued"));
        Assert.Equal(MessageDeliveryVisualState.Pending, MainWindowPresentation.MapMessageDeliveryVisualState("sending"));
        Assert.Equal(MessageDeliveryVisualState.Pending, MainWindowPresentation.MapMessageDeliveryVisualState("retrying"));
        Assert.Equal(MessageDeliveryVisualState.Sent, MainWindowPresentation.MapMessageDeliveryVisualState("sent"));
        Assert.Equal(MessageDeliveryVisualState.Delivered, MainWindowPresentation.MapMessageDeliveryVisualState("delivered"));
        Assert.Equal(MessageDeliveryVisualState.Error, MainWindowPresentation.MapMessageDeliveryVisualState("failed"));
        Assert.Equal(MessageDeliveryVisualState.Error, MainWindowPresentation.MapMessageDeliveryVisualState("expired"));
    }

    [Fact]
    public void MessageDeliveryPresentation_UsesChecksClockAndErrorGlyphs()
    {
        Assert.Equal("\uE823", MainWindowPresentation.BuildMessageDeliveryPrimaryGlyph(MessageDeliveryVisualState.Pending));
        Assert.Equal("\uE8FB", MainWindowPresentation.BuildMessageDeliveryPrimaryGlyph(MessageDeliveryVisualState.Sent));
        Assert.Equal("\uE8FB", MainWindowPresentation.BuildMessageDeliveryPrimaryGlyph(MessageDeliveryVisualState.Delivered));
        Assert.Equal("\uE8C9", MainWindowPresentation.BuildMessageDeliveryPrimaryGlyph(MessageDeliveryVisualState.Error));
        Assert.Equal("\uE8FB", MainWindowPresentation.BuildMessageDeliverySecondaryGlyph(MessageDeliveryVisualState.Delivered));
        Assert.Equal(Visibility.Visible, MainWindowPresentation.BuildMessageDeliverySecondaryGlyphVisibility(MessageDeliveryVisualState.Delivered));
        Assert.Equal(Visibility.Collapsed, MainWindowPresentation.BuildMessageDeliverySecondaryGlyphVisibility(MessageDeliveryVisualState.Sent));
    }

    [Fact]
    public void MessageDeliveryPresentation_KeepsDistinctSemanticTextForErrorFamily()
    {
        Assert.Equal("Ошибка отправки", MainWindowPresentation.MapMessageStatus("failed"));
        Assert.Equal("Срок ожидания истёк", MainWindowPresentation.MapMessageStatus("expired"));
        Assert.Equal("Отправлено, ждём подтверждения", MainWindowPresentation.MapMessageStatus("sent"));
        Assert.Equal(
            "Статус сообщения: Доставлено",
            MainWindowPresentation.BuildMessageDeliveryAutomationName("Доставлено"));
    }

    [Fact]
    public void CalculateTranscriptBubbleMaxWidth_UsesAvailableWidthRatio()
    {
        double width = MainWindowPresentation.CalculateTranscriptBubbleMaxWidth(500d, 24d);

        Assert.Equal(342.72d, width, 2);
    }

    [Fact]
    public void CalculateTranscriptBubbleMaxWidth_ClampsWideHostsToHardCap()
    {
        double width = MainWindowPresentation.CalculateTranscriptBubbleMaxWidth(1000d, 24d);

        Assert.Equal(620d, width);
    }

    [Fact]
    public void CalculateTranscriptBubbleMaxWidth_FallsBackForInvalidHostWidth()
    {
        Assert.Equal(620d, MainWindowPresentation.CalculateTranscriptBubbleMaxWidth(0d, 24d));
        Assert.Equal(620d, MainWindowPresentation.CalculateTranscriptBubbleMaxWidth(double.NaN, 24d));
    }

    // Один одобренный, знакомый контакт для проверок подписи в списке. Presence задаётся
    // вызывающим через `with`, чтобы каждый тест менял ровно то, что проверяет.
    private static ContactSnapshot ApprovedPeerContact(long nowUnixMs) =>
        new(
            ContactId: "contact-1",
            Petname: "Peer",
            RemoteAccountId: "acct-peer",
            PublicDisplayName: "Peer",
            PublicAvatarHashHex: null,
            PublicAvatarMimeType: null,
            PublicAvatarImagePath: null,
            PublicAvatarPendingHashHex: null,
            PublicAvatarPendingMimeType: null,
            PublicProfileUpdatedAtUnixMs: null,
            RemoteInstanceId: "inst-peer",
            PrimaryDialogKey: "dialog:instance:inst-peer",
            RelationshipState: "approved",
            PendingRequestDirection: null,
            HasMet: true,
            FriendRelayConsent: false,
            IsBlocked: false,
            PresenceState: "mystery",
            PresenceLastSeenAtUnixMs: null,
            PresenceLastSentAtUnixMs: null,
            PresenceLastActivityAtUnixMs: nowUnixMs - 30_000,
            RemovedAtUnixMs: null,
            CreatedAtUnixMs: nowUnixMs - 60_000,
            UpdatedAtUnixMs: nowUnixMs - 30_000
        );

    [Theory]
    [InlineData("Олег", "acct-e929d200660d5ce1", null, "Олег")]
    [InlineData("acct-e929d200660d5ce1", "acct-e929d200660d5ce1", "PC", "PC")]
    [InlineData("acct-e929d200660d5ce1", "acct-e929d200660d5ce1", null, "Без имени · e929…")]
    [InlineData("  ", "acct-e929d200660d5ce1", "", "Без имени · e929…")]
    public void ContactTitle_NeverShowsTheAccountIdAsAName(string petname, string accountId, string? publicName, string expected)
        => Assert.Equal(
            expected,
            MainWindowPresentation.ContactTitle(ApprovedPeerContact(0) with
            {
                Petname = petname,
                RemoteAccountId = accountId,
                PublicDisplayName = publicName,
            }));

    [Fact]
    public void BuildContactStatus_DoesNotPromoteUnknownRecentPresenceToOnline()
    {
        long nowUnixMs = 1_000_000;
        ContactSnapshot contact = ApprovedPeerContact(nowUnixMs);

        string status = MainWindowPresentation.BuildContactStatus(
            contact,
            nowUnixMs,
            new HashSet<string>());

        Assert.Equal("Недавно в сети", status);
    }

    [Fact]
    public void BuildContactStatus_SaysOnlineOnlyWhileALiveLinkIsHeld()
    {
        // "В сети" обязано значить то же, что зелёная точка — живой линк прямо сейчас. Иначе в
        // списке контактов «В сети» соседствует с «нужен relay» в шапке чата и с сообщением,
        // застрявшим на «ждём подтверждения». См. reachability-belongs-to-the-session.md.
        long nowUnixMs = 1_700_000_000_000;
        ContactSnapshot contact = ApprovedPeerContact(nowUnixMs) with { PresenceState = "online" };

        Assert.Equal(
            "Недавно в сети",
            MainWindowPresentation.BuildContactStatus(contact, nowUnixMs, new HashSet<string>()));
        Assert.Equal(
            "В сети",
            MainWindowPresentation.BuildContactStatus(
                contact,
                nowUnixMs,
                new HashSet<string> { "acct-peer" }));
    }
    [Fact]
    public void MapContactPresenceDotKind_IsLiveOnlyWhileALinkIsHeld()
    {
        long nowUnixMs = 1_700_000_000_000;
        ContactSnapshot contact = ApprovedPeerContact(nowUnixMs) with { PresenceState = "online" };

        Assert.Equal(
            ContactPresenceDotKind.Recent,
            MainWindowPresentation.MapContactPresenceDotKind(contact, nowUnixMs, new HashSet<string>()));
        Assert.Equal(
            ContactPresenceDotKind.Live,
            MainWindowPresentation.MapContactPresenceDotKind(
                contact,
                nowUnixMs,
                new HashSet<string> { "acct-peer" }));
    }

    [Fact]
    public void BuildContactItemSignature_ChangesWhenTheLinkDrops()
    {
        // Отправная точка — живой снимок: на ПК в списке горела зелёная точка, пока вторая машина
        // была ВЫКЛЮЧЕНА, а шапка рядом писала «Офлайн». Причина не в расчёте точки (он честный),
        // а в подписи, по которой список решает, перерисовываться ли: полей контакта в ней
        // достаточно, пока пир жив, но выключенный пир просто перестаёт обновлять присутствие —
        // поля замирают, подпись совпадает, строка не перерисовывается.
        long nowUnixMs = 1_700_000_000_000;
        ContactSnapshot contact = ApprovedPeerContact(nowUnixMs) with { PresenceState = "online" };
        var linked = new HashSet<string> { "acct-peer" };

        string live = MainWindowPresentation.BuildContactItemSignature(
            contact,
            MainWindowPresentation.MapContactPresenceDotKind(contact, nowUnixMs, linked),
            MainWindowPresentation.BuildContactStatus(contact, nowUnixMs, linked));
        string dropped = MainWindowPresentation.BuildContactItemSignature(
            contact,
            MainWindowPresentation.MapContactPresenceDotKind(contact, nowUnixMs, new HashSet<string>()),
            MainWindowPresentation.BuildContactStatus(contact, nowUnixMs, new HashSet<string>()));

        Assert.NotEqual(live, dropped);
    }

    [Fact]
    public void A_Date_Reads_As_A_Person_Says_It()
    {
        var now = new DateTimeOffset(2026, 10, 13, 12, 0, 0, TimeZoneInfo.Local.GetUtcOffset(new DateTime(2026, 10, 13)));
        long At(int month, int day, int hour, int minute, int year = 2026) =>
            new DateTimeOffset(year, month, day, hour, minute, 0, now.Offset).ToUnixTimeMilliseconds();
        Assert.Equal("сегодня в 9:05", MainWindowPresentation.FormatHumanDate(At(10, 13, 9, 5), now));
        Assert.Equal("вчера в 23:17", MainWindowPresentation.FormatHumanDate(At(10, 12, 23, 17), now));
        Assert.Equal("1 октября", MainWindowPresentation.FormatHumanDate(At(10, 1, 8, 0), now));
        Assert.Equal("3 марта 2025", MainWindowPresentation.FormatHumanDate(At(3, 3, 8, 0, 2025), now));
    }

    [Fact]
    public void BuildContactItemSignature_HoldsThroughAHeartbeat()
    {
        // The other way round: a heartbeat moves the presence clocks but not what the row shows. When
        // the clocks were in the signature the row was replaced every few seconds and the contact in the
        // list «дёрнулся» (owner, 2026-10-13).
        long nowUnixMs = 1_700_000_000_000;
        ContactSnapshot contact = ApprovedPeerContact(nowUnixMs) with { PresenceState = "online" };
        ContactSnapshot heartbeat = contact with
        {
            PresenceLastSeenAtUnixMs = nowUnixMs + 5_000,
            PresenceLastActivityAtUnixMs = nowUnixMs + 5_000,
        };
        var linked = new HashSet<string> { "acct-peer" };

        string Signature(ContactSnapshot c) => MainWindowPresentation.BuildContactItemSignature(
            c,
            MainWindowPresentation.MapContactPresenceDotKind(c, nowUnixMs + 5_000, linked),
            MainWindowPresentation.BuildContactStatus(c, nowUnixMs + 5_000, linked));

        Assert.Equal(Signature(contact), Signature(heartbeat));
    }

    [Fact]
    public void PresenceTextAndDotCannotDisagree()
    {
        // Пользователь прислал скриншот: подпись «В сети» и ОРАНЖЕВАЯ точка рядом. Расходились они
        // потому, что считались из разного: слово приходило из ядра (объявленное присутствие пира),
        // точка — из живого линка. Инвариант теперь один: обе — проекции одного решения.
        long nowUnixMs = 1_700_000_000_000;
        var linked = new HashSet<string> { "acct-peer" };
        var notLinked = new HashSet<string>();

        // Самый коварный случай: пир объявил себя офлайн, но линк с ним живой.
        ContactSnapshot staleOffline = ApprovedPeerContact(nowUnixMs) with { PresenceState = "offline" };
        ContactPresenceDotKind liveKind =
            MainWindowPresentation.MapContactPresenceDotKind(staleOffline, nowUnixMs, linked);
        Assert.Equal(ContactPresenceDotKind.Live, liveKind);
        Assert.Equal("В сети", MainWindowPresentation.MapContactPresenceText(staleOffline, liveKind));

        // И обратный: пир объявил себя в сети, а линка нет.
        ContactSnapshot staleOnline = ApprovedPeerContact(nowUnixMs) with { PresenceState = "online" };
        ContactPresenceDotKind recentKind =
            MainWindowPresentation.MapContactPresenceDotKind(staleOnline, nowUnixMs, notLinked);
        Assert.NotEqual(ContactPresenceDotKind.Live, recentKind);
        Assert.NotEqual("В сети", MainWindowPresentation.MapContactPresenceText(staleOnline, recentKind));
    }

    [Fact]
    public void BuildPeerSubtitle_NeverClaimsAConnectionItCannotSee()
    {
        // Функция знает только выбранный диалог — про достижимость ничего.
        // Раньше она на этом основании писала «Активный чат подключён».
        Assert.Equal("Диалог пока не выбран", MainWindowPresentation.BuildPeerSubtitle(null));
        Assert.Equal(string.Empty, MainWindowPresentation.BuildPeerSubtitle("dialog:instance:peer"));
    }

    [Fact]
    public void MapChatSessionState_MapsConnected()
        => Assert.Equal("подключена", MainWindowPresentation.MapChatSessionState("connected"));

    [Fact]
    public void MapChatSessionState_MapsPaused()
        => Assert.Equal("завершена пользователем", MainWindowPresentation.MapChatSessionState("paused"));

    [Fact]
    public void BuildChatSessionPingText_UsesRoundTrip()
    {
        ChatSessionSummary summary = new(
            IsOpenLease: true,
            AutoReconnectPaused: false,
            SessionStateValue: "connected",
            PeerEndpoint: "192.168.0.12:5050",
            LocalEndpoint: "192.168.0.10:4040",
            LastInteractionAtUnixMs: 1_000_000,
            LastInteractionKindValue: "keepalive",
            LastConfirmedKeepaliveAtUnixMs: 995_000,
            LastRoundTripMs: 42);

        Assert.Equal("Пинг: 42 мс", MainWindowPresentation.BuildChatSessionPingText(summary));
    }

    [Fact]
    public void BuildChatSessionPingText_UsesDashWithoutRoundTrip()
    {
        ChatSessionSummary summary = new(
            IsOpenLease: true,
            AutoReconnectPaused: false,
            SessionStateValue: "connected",
            PeerEndpoint: "192.168.0.12:5050",
            LocalEndpoint: "192.168.0.10:4040",
            LastInteractionAtUnixMs: 1_000_000,
            LastInteractionKindValue: "keepalive",
            LastConfirmedKeepaliveAtUnixMs: null,
            LastRoundTripMs: null);

        Assert.Equal("Пинг: —", MainWindowPresentation.BuildChatSessionPingText(summary));
    }

    [Fact]
    public void BuildChatSessionKeepaliveText_UsesConfirmedKeepaliveAge()
    {
        ChatSessionSummary summary = new(
            IsOpenLease: true,
            AutoReconnectPaused: false,
            SessionStateValue: "connected",
            PeerEndpoint: "192.168.0.12:5050",
            LocalEndpoint: "192.168.0.10:4040",
            LastInteractionAtUnixMs: 970_000,
            LastInteractionKindValue: "keepalive",
            LastConfirmedKeepaliveAtUnixMs: 995_000,
            LastRoundTripMs: 42);

        string text = MainWindowPresentation.BuildChatSessionKeepaliveText(summary, 1_000_000);

        Assert.Equal("Keepalive: 5 сек назад", text);
    }

    /// Стенд 2026-10-14: «Пинг 4 мс» до Марка читался как его далёкий ПК, а отвечал его ноутбук на
    /// этом же компьютере. Строка говорит, какое из устройств и где оно.
    [Theory]
    [InlineData("192.168.1.37:50236", 2, "Устройство собеседника: одно из 2, на этом же компьютере")]
    [InlineData("192.168.1.80:50236", 2, "Устройство собеседника: одно из 2, в локальной сети")]
    [InlineData("192.0.2.144:56508", 1, "Устройство собеседника: единственное, через интернет")]
    public void BuildChatSessionPeerDeviceText_SaysWhichDeviceAndWhere(string peer, int count, string expected)
    {
        ChatSessionSummary summary = new(
            IsOpenLease: true,
            AutoReconnectPaused: false,
            SessionStateValue: "connected",
            PeerEndpoint: peer,
            LocalEndpoint: null,
            LastInteractionAtUnixMs: null,
            LastInteractionKindValue: null,
            LastConfirmedKeepaliveAtUnixMs: null,
            LastRoundTripMs: 4,
            PeerDeviceId: "dev-b",
            PeerDeviceCount: count);
        NetworkInterfaceSnapshot ethernet = new(
            "eth0", "Ethernet", IsUp: true, IsVirtual: false, IsLoopback: false, SupportsBroadcast: true,
            SupportsMulticast: true, IPv4Addresses: ["192.168.1.37"], PrefixLengths: [24]);

        Assert.Equal(expected, MainWindowPresentation.BuildChatSessionPeerDeviceText(summary, [ethernet]));
    }

    /// «Сидеть через него»: плашка говорит, держится ли релей, и что будет, пока его нет — по галочке
    /// «не открывать мой адрес».
    [Fact]
    public void DescribePinnedRelay_SaysWhetherItHoldsAndWhatHappensWithout()
    {
        Assert.Equal(Microsoft.UI.Xaml.Controls.InfoBarSeverity.Success,
            MainWindowPresentation.DescribePinnedRelay(linked: true, lostSecs: null, keepAddressHidden: false, always: false).Severity);
        Assert.Equal("Подключаюсь к релею…",
            MainWindowPresentation.DescribePinnedRelay(false, null, false, false).Message);
        string open = MainWindowPresentation.DescribePinnedRelay(false, 125, keepAddressHidden: false, always: true).Message;
        Assert.Contains("2 мин", open);
        Assert.Contains("и напрямую тоже", open);
        Assert.Contains("в каталоге", open);
        string hidden = MainWindowPresentation.DescribePinnedRelay(false, 30, keepAddressHidden: true, always: false).Message;
        Assert.Contains("Адрес не открываю", hidden);
        Assert.DoesNotContain("в каталоге", hidden);
    }

    [Fact]
    public void BuildChatSessionPeerDeviceText_IsSilentWithoutALink()
    {
        ChatSessionSummary summary = new(true, false, "reconnecting", null, null, null, null, null, null);

        Assert.Null(MainWindowPresentation.BuildChatSessionPeerDeviceText(summary, []));
    }

    [Fact]
    public void BuildChatSessionNetworkLabel_UsesLoopbackForLoopbackEndpoints()
    {
        ChatSessionSummary summary = new(
            IsOpenLease: true,
            AutoReconnectPaused: false,
            SessionStateValue: "connected",
            PeerEndpoint: "127.0.0.1:5050",
            LocalEndpoint: "127.0.0.1:4040",
            LastInteractionAtUnixMs: null,
            LastInteractionKindValue: null,
            LastConfirmedKeepaliveAtUnixMs: null,
            LastRoundTripMs: null);

        string label = MainWindowPresentation.BuildChatSessionNetworkLabel(summary, []);

        Assert.Equal("Loopback (127.0.0.1)", label);
    }

    /// Живой экран 2026-08-31: «Сеть: d43dcaae…444 • Endpoint: d43dcaae…444» — одно значение
    /// в двух полях, и это идентификатор УЗЛА, а не сеть. Причину чинит ядро (в summary теперь
    /// адрес соединения), но и сама метка не имеет права повторять соседнее поле.
    [Fact]
    public void BuildChatSessionNetworkLabel_NeverEchoesTheEndpointItCannotClassify()
    {
        const string nodeAddress = "d43dcaaeb12e411f1f1a5a9b6f7414ab0628d444";
        ChatSessionSummary summary = new(
            IsOpenLease: true,
            AutoReconnectPaused: false,
            SessionStateValue: "connected",
            PeerEndpoint: nodeAddress,
            // Под mesh локального адреса в summary не было — ровно поэтому лестница доходила до
            // последней строки и печатала чужой endpoint.
            LocalEndpoint: null,
            LastInteractionAtUnixMs: null,
            LastInteractionKindValue: null,
            LastConfirmedKeepaliveAtUnixMs: null,
            LastRoundTripMs: null);

        string label = MainWindowPresentation.BuildChatSessionNetworkLabel(summary, []);

        Assert.DoesNotContain(nodeAddress, label);
        Assert.Equal("—", label);
        // И отрицательная половина: то, что классифицировать МОЖНО, по-прежнему классифицируется,
        // иначе «—» просто съел бы полезный ответ.
        Assert.Equal(
            "Интернет",
            MainWindowPresentation.BuildChatSessionNetworkLabel(
                summary with { PeerEndpoint = "203.0.113.108:27406" },
                []));
    }

    [Fact]
    public void BuildChatSessionNetworkLabel_UsesInternetForPublicPeerEndpoint()
    {
        ChatSessionSummary summary = new(
            IsOpenLease: true,
            AutoReconnectPaused: false,
            SessionStateValue: "connected",
            PeerEndpoint: "/ip4/198.51.100.234/udp/22294/quic-v1",
            LocalEndpoint: "/ip4/127.0.0.1/udp/51334/quic-v1",
            LastInteractionAtUnixMs: null,
            LastInteractionKindValue: null,
            LastConfirmedKeepaliveAtUnixMs: null,
            LastRoundTripMs: null);

        string label = MainWindowPresentation.BuildChatSessionNetworkLabel(summary, []);

        Assert.Equal("Интернет", label);
    }

    [Fact]
    public void BuildChatSessionNetworkLabel_UsesInterfaceForSamePrefixIpv6PeerEndpoint()
    {
        ChatSessionSummary summary = new(
            IsOpenLease: true,
            AutoReconnectPaused: false,
            SessionStateValue: "connected",
            PeerEndpoint: "[2a00:1e88:d3a4:a000:c0fa:59cf:b9bb:28e2]:58470",
            LocalEndpoint: "[::]:58469",
            LastInteractionAtUnixMs: null,
            LastInteractionKindValue: null,
            LastConfirmedKeepaliveAtUnixMs: null,
            LastRoundTripMs: null);
        NetworkInterfaceSnapshot network = new(
            Id: "wifi-1",
            DisplayName: "Wi-Fi",
            IsUp: true,
            IsVirtual: false,
            IsLoopback: false,
            SupportsBroadcast: true,
            SupportsMulticast: true,
            IPv4Addresses: ["192.168.7.21"],
            PrefixLengths: [24],
            IPv6Addresses: ["2a00:1e88:d3a4:a000:8ac0:99ff:fe12:3456"]);

        string label = MainWindowPresentation.BuildChatSessionNetworkLabel(summary, [network]);

        Assert.Equal("Wi-Fi (192.168.7.21/24)", label);
    }

    [Fact]
    public void BuildChatSessionNetworkLabel_UsesInterfaceDisplayNameForMatchingEndpoint()
    {
        ChatSessionSummary summary = new(
            IsOpenLease: true,
            AutoReconnectPaused: false,
            SessionStateValue: "connected",
            PeerEndpoint: "192.168.0.12:5050",
            LocalEndpoint: "192.168.0.10:4040",
            LastInteractionAtUnixMs: null,
            LastInteractionKindValue: null,
            LastConfirmedKeepaliveAtUnixMs: null,
            LastRoundTripMs: null);
        NetworkInterfaceSnapshot network = new(
            Id: "if-1",
            DisplayName: "Ethernet",
            IsUp: true,
            IsVirtual: false,
            IsLoopback: false,
            SupportsBroadcast: true,
            SupportsMulticast: true,
            IPv4Addresses: ["192.168.0.10"],
            PrefixLengths: [24]);

        string label = MainWindowPresentation.BuildChatSessionNetworkLabel(summary, [network]);

        Assert.Equal("Ethernet (192.168.0.10/24)", label);
    }

    [Fact]
    public void BuildChatSessionNetworkLabel_UsesInventoryStyleForMatchingMultiaddrEndpoint()
    {
        ChatSessionSummary summary = new(
            IsOpenLease: true,
            AutoReconnectPaused: false,
            SessionStateValue: "connected",
            PeerEndpoint: "/ip4/192.168.0.12/udp/5050/quic-v1",
            LocalEndpoint: "/ip4/192.168.0.10/udp/4040/quic-v1",
            LastInteractionAtUnixMs: null,
            LastInteractionKindValue: null,
            LastConfirmedKeepaliveAtUnixMs: null,
            LastRoundTripMs: null);
        NetworkInterfaceSnapshot network = new(
            Id: "if-2",
            DisplayName: "Ethernet",
            IsUp: true,
            IsVirtual: false,
            IsLoopback: false,
            SupportsBroadcast: true,
            SupportsMulticast: true,
            IPv4Addresses: ["192.168.0.10"],
            PrefixLengths: [24]);

        string label = MainWindowPresentation.BuildChatSessionNetworkLabel(summary, [network]);

        Assert.Equal("Ethernet (192.168.0.10/24)", label);
    }

    [Fact]
    public void BuildChatSessionNetworkLabel_FallsBackToLocalEndpoint()
    {
        ChatSessionSummary summary = new(
            IsOpenLease: true,
            AutoReconnectPaused: false,
            SessionStateValue: "connected",
            PeerEndpoint: "192.168.0.12:5050",
            LocalEndpoint: "10.0.0.8:4040",
            LastInteractionAtUnixMs: null,
            LastInteractionKindValue: null,
            LastConfirmedKeepaliveAtUnixMs: null,
            LastRoundTripMs: null);

        string label = MainWindowPresentation.BuildChatSessionNetworkLabel(summary, []);

        Assert.Equal("10.0.0.8:4040", label);
    }

    [Fact]
    public void BuildChatSessionNetworkLabel_FallsBackToPeerSubnetWhenLocalEndpointMissing()
    {
        ChatSessionSummary summary = new(
            IsOpenLease: true,
            AutoReconnectPaused: false,
            SessionStateValue: "connected",
            PeerEndpoint: "/ip4/192.168.7.113/udp/51326/quic-v1",
            LocalEndpoint: null,
            LastInteractionAtUnixMs: null,
            LastInteractionKindValue: null,
            LastConfirmedKeepaliveAtUnixMs: null,
            LastRoundTripMs: null);
        NetworkInterfaceSnapshot network = new(
            Id: "wifi-1",
            DisplayName: "Беспроводная сеть (Intel(R) Wi-Fi 7 BE200 320MHz)",
            IsUp: true,
            IsVirtual: false,
            IsLoopback: false,
            SupportsBroadcast: true,
            SupportsMulticast: true,
            IPv4Addresses: ["192.168.7.21"],
            PrefixLengths: [24]);

        string label = MainWindowPresentation.BuildChatSessionNetworkLabel(summary, [network]);

        Assert.Equal("Беспроводная сеть (Intel(R) Wi-Fi 7 BE200 320MHz) (192.168.7.21/24)", label);
    }

    [Fact]
    public void BuildChatSessionEndpointText_NormalizesMultiaddr()
    {
        ChatSessionSummary summary = new(
            IsOpenLease: true,
            AutoReconnectPaused: false,
            SessionStateValue: "connected",
            PeerEndpoint: "/ip4/192.168.7.113/udp/51326/quic-v1",
            LocalEndpoint: null,
            LastInteractionAtUnixMs: null,
            LastInteractionKindValue: null,
            LastConfirmedKeepaliveAtUnixMs: null,
            LastRoundTripMs: null);

        string text = MainWindowPresentation.BuildChatSessionEndpointText(summary);

        Assert.Equal("Endpoint: 192.168.7.113:51326", text);
    }

    [Fact]
    public void BuildTransportDiagnostics_SaysHowTheNodeAndItsContactsAreLinked()
    {
        TransportSnapshot snapshot = new(
            State: "connected",
            TransportModeValue: "quic",
            PeerId: "4f1c0d2e9a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d",
            PeerKey: "dialog:instance:pc",
            RemoteInstanceId: null,
            RemoteAccountId: "acct-pc",
            RemoteDeviceName: "PC",
            InitiatedLocally: true,
            LastError: null);
        MeshConnectionStateDto mesh = new(
            new NodeConnectivityDto("online_behind_nat", null),
            [
                Contact("acct-pc", "online_direct", "quic"),
                Contact("acct-tmp", "online_via_relay", "tcp_door"),
                Contact("acct-old", "searching", null),
            ]);

        string diagnostics = MainWindowPresentation.BuildTransportDiagnostics(snapshot, mesh);

        Assert.Contains("Узел в сети: в сети, за NAT", diagnostics);
        Assert.Contains("напрямую 1, через сеть 1, ищем 1 (из 3)", diagnostics);
        Assert.Contains("QUIC 1, TCP-дверь 1", diagnostics);
        Assert.Contains("Активная сессия: PC (4f1c0d2e9a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d)", diagnostics);
        // The old listener's lines are gone: nothing fills them any more.
        Assert.DoesNotContain("Порт приёма", diagnostics);
        Assert.DoesNotContain("Bind", diagnostics);
    }

    private static ContactConnectionDto Contact(string accountId, string link, string? carrier)
        => new(accountId, link, null, carrier, null, true, null, false, null);
}
