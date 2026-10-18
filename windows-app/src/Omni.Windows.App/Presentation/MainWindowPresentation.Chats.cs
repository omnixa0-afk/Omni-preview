using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Chats;

namespace Omni.Windows.App.Presentation;

internal static partial class MainWindowPresentation
{
    public static string MapMessageStatus(string status)
        => status.ToLowerInvariant() switch
        {
            "queued" => "В очереди",
            "sending" => "Отправляется",
            "sent" => "Отправлено, ждём подтверждения",
            "delivered" => "Доставлено",
            "retrying" => "Повторная отправка",
            "failed" => "Ошибка отправки",
            "expired" => "Срок ожидания истёк",
            _ => "Неизвестный статус",
        };

    internal static MessageDeliveryVisualState MapMessageDeliveryVisualState(string status)
        => status.ToLowerInvariant() switch
        {
            "queued" or "sending" or "retrying" => MessageDeliveryVisualState.Pending,
            "sent" => MessageDeliveryVisualState.Sent,
            "delivered" => MessageDeliveryVisualState.Delivered,
            "failed" or "expired" => MessageDeliveryVisualState.Error,
            _ => MessageDeliveryVisualState.None,
        };

    internal static string BuildMessageDeliveryPrimaryGlyph(MessageDeliveryVisualState state)
        => state switch
        {
            MessageDeliveryVisualState.Pending => "\uE823",
            MessageDeliveryVisualState.Sent => "\uE8FB",
            MessageDeliveryVisualState.Delivered => "\uE8FB",
            MessageDeliveryVisualState.Error => "\uE8C9",
            MessageDeliveryVisualState.Read => "\uE8FB",
            _ => string.Empty,
        };

    internal static string BuildMessageDeliverySecondaryGlyph(MessageDeliveryVisualState state)
        => state is MessageDeliveryVisualState.Delivered or MessageDeliveryVisualState.Read
            ? "\uE8FB"
            : string.Empty;

    internal static Visibility BuildMessageDeliveryIndicatorVisibility(bool outgoing, MessageDeliveryVisualState state)
        => outgoing && state != MessageDeliveryVisualState.None
            ? Visibility.Visible
            : Visibility.Collapsed;

    internal static Visibility BuildMessageDeliveryNeutralVisibility(bool outgoing, MessageDeliveryVisualState state)
        => outgoing && state != MessageDeliveryVisualState.None && state != MessageDeliveryVisualState.Error
            ? Visibility.Visible
            : Visibility.Collapsed;

    internal static Visibility BuildMessageDeliveryErrorVisibility(bool outgoing, MessageDeliveryVisualState state)
        => outgoing && state == MessageDeliveryVisualState.Error
            ? Visibility.Visible
            : Visibility.Collapsed;

    internal static Visibility BuildMessageDeliverySecondaryGlyphVisibility(MessageDeliveryVisualState state)
        => state is MessageDeliveryVisualState.Delivered or MessageDeliveryVisualState.Read
            ? Visibility.Visible
            : Visibility.Collapsed;

    internal static Thickness BuildMessageDeliveryPrimaryGlyphMargin(MessageDeliveryVisualState state)
        => state is MessageDeliveryVisualState.Delivered or MessageDeliveryVisualState.Read
            ? new Thickness(-4, 0, 4, 0)
            : new Thickness(0);

    internal static Thickness BuildMessageDeliverySecondaryGlyphMargin(MessageDeliveryVisualState state)
        => state is MessageDeliveryVisualState.Delivered or MessageDeliveryVisualState.Read
            ? new Thickness(4, 0, -4, 0)
            : new Thickness(0);

    internal static bool IsMessageDeliveryError(MessageDeliveryVisualState state)
        => state == MessageDeliveryVisualState.Error;

    internal static string BuildMessageDeliveryToolTipText(string? semanticStatusText)
        => semanticStatusText ?? string.Empty;

    internal static string BuildMessageDeliveryAutomationName(string? semanticStatusText)
        => string.IsNullOrWhiteSpace(semanticStatusText)
            ? string.Empty
            : $"Статус сообщения: {semanticStatusText}";

    internal static double CalculateTranscriptBubbleMaxWidth(double hostWidth, double horizontalPadding)
    {
        if (double.IsNaN(hostWidth) || double.IsInfinity(hostWidth) || hostWidth <= 0d)
        {
            return TranscriptBubbleMaxWidthHardCap;
        }

        if (double.IsNaN(horizontalPadding) || double.IsInfinity(horizontalPadding))
        {
            horizontalPadding = 0d;
        }

        double availableWidth = hostWidth - Math.Max(0d, horizontalPadding);
        if (availableWidth <= 0d)
        {
            return TranscriptBubbleMaxWidthHardCap;
        }

        return Math.Min(TranscriptBubbleMaxWidthHardCap, availableWidth * TranscriptBubbleWidthFactor);
    }

    internal static ChatMessageListItem CreateChatItem(
        ChatMessageRecord message,
        MessageSecurityDetails? securityDetails,
        ChatSecuritySummary? chatSecuritySummary,
        ContactCardSnapshot? linkedContact)
    {
        ChatMessageListItem item = new(message.Id);
        item.ApplyFrom(message, securityDetails, chatSecuritySummary, linkedContact);
        return item;
    }

    public static string BuildChatSecuritySummary(ChatSecuritySummary? summary, ContactCardSnapshot? card)
    {
        if (summary is null)
        {
            return "Данные безопасности для текущего диалога пока недоступны.";
        }

        List<string> lines =
        [
            $"Статус соединения: {BuildChatConnectionStatus(summary, card)}",
            $"Шифрование канала: {(summary.UsesEncryptedTransport ? "включено" : "не включено")}",
            $"Сессия: {(summary.HasEstablishedSession ? "готова" : "ещё не установлена")}",
            $"Состояние bootstrap: {MapSessionState(summary.SessionState)}",
            $"Последняя активность сессии: {(summary.LastSessionUpdateUnixMs.HasValue ? FormatTimestamp(summary.LastSessionUpdateUnixMs.Value) : "нет")}",
            $"Состояние доверия: {MapTrustState(card?.TrustState ?? summary.TrustState ?? "unverified")}",
            $"Предупреждение безопасности: {MapSecurityWarning(summary.Warning)}",
        ];

        if (!string.IsNullOrWhiteSpace(summary.RemoteAccountId))
        {
            lines.Add($"Удалённый аккаунт: {summary.RemoteAccountId}");
        }

        if (!string.IsNullOrWhiteSpace(summary.RemoteDeviceId))
        {
            lines.Add($"Удалённое устройство: {summary.RemoteDeviceId}");
        }

        return string.Join('\n', lines);
    }

    public static string BuildChatConnectionStatus(ChatSecuritySummary? summary, ContactCardSnapshot? card)
    {
        if (summary is null)
        {
            return "Статус: диалог не выбран";
        }

        if (card is null)
        {
            return "Статус: диалог не привязан";
        }

        if (card.Contact.RelationshipState.Equals("declined", StringComparison.OrdinalIgnoreCase))
        {
            return "Статус: контакт отклонён";
        }

        if (card.Contact.RelationshipState.Equals("pending", StringComparison.OrdinalIgnoreCase))
        {
            return $"Статус: {BuildPendingContactStatus(card.Contact).ToLowerInvariant()}";
        }

        if ((summary.Warning ?? string.Empty).ToLowerInvariant() is "blockedcontact" or "blocked_contact")
        {
            return "Статус: контакт отклонён";
        }

        if (IsAccountDialogAwaitingFirstConnection(summary, card))
        {
            return "Статус: ждём первое подключение";
        }

        if (summary.HasEstablishedSession)
        {
            return "Статус: зашифровано";
        }

        if (summary.HasRemotePrekeyBundle)
        {
            return "Статус: подключение";
        }

        return "Статус: обмен ключами";
    }

    public static string BuildSecurityDiagnostics(ChatSecuritySummary? summary, ContactCardSnapshot? card)
    {
        if (summary is null)
        {
            return "Для текущего диалога ещё нет сводки безопасности.";
        }

        return $"Состояние доверия: {MapTrustState(card?.TrustState ?? summary.TrustState ?? "unverified")}\n" +
               $"Предупреждение безопасности: {MapSecurityWarning(summary.Warning)}\n" +
               $"Статус соединения: {BuildChatConnectionStatus(summary, card)}\n" +
               $"Сессия присутствует: {(summary.HasEstablishedSession ? "да" : "нет")}\n" +
               $"Состояние bootstrap: {MapSessionState(summary.SessionState)}\n" +
               $"Последняя активность сессии: {(summary.LastSessionUpdateUnixMs.HasValue ? FormatTimestamp(summary.LastSessionUpdateUnixMs.Value) : "нет")}\n" +
               $"Шифрование канала: {(summary.UsesEncryptedTransport ? "включено" : "не включено")}";
    }

    public static string BuildQueueDiagnostics(QueueDiagnosticsSnapshot? snapshot, string? currentDialogKey)
    {
        if (snapshot is null)
        {
            return "Очередь сообщений недоступна.";
        }

        DialogQueueDiagnostics? currentDialog = string.IsNullOrWhiteSpace(currentDialogKey)
            ? null
            : snapshot.PendingByDialog.FirstOrDefault(item =>
                string.Equals(item.DialogKey, currentDialogKey, StringComparison.OrdinalIgnoreCase));

        List<string> lines =
        [
            $"Всего в очереди: {snapshot.QueueLength}",
            $"В очереди: {snapshot.QueuedCount}",
            $"Отправляется: {snapshot.SendingCount}",
            $"Ждут подтверждение: {snapshot.SentWaitingAckCount}",
            $"Повторная отправка: {snapshot.RetryingCount}",
            $"Ошибок: {snapshot.FailedCount}",
            $"Последняя ошибка доставки: {FormatOptional(snapshot.LastDeliveryError)}",
            $"Последняя причина сбоя: {FormatFailure(snapshot.LastFailureKind, snapshot.LastFailureMessage)}",
        ];

        if (currentDialog is not null)
        {
            lines.Add(string.Empty);
            lines.Add("Текущий диалог:");
            lines.Add($"Ожидают отправку: {currentDialog.PendingCount}");
            lines.Add($"Ждут подтверждение: {currentDialog.SentWaitingAckCount}");
            lines.Add($"Повторная отправка: {currentDialog.RetryingCount}");
            lines.Add($"Ошибок: {currentDialog.FailedCount}");
            lines.Add($"Причина: {FormatFailure(currentDialog.LastFailureKind, currentDialog.LastFailureMessage)}");
        }

        return string.Join('\n', lines);
    }

    private static string BuildMessageDetails(
        ChatMessageRecord message,
        MessageSecurityDetails? securityDetails,
        ChatSecuritySummary? chatSecuritySummary,
        ContactCardSnapshot? linkedContact)
    {
        List<string> lines =
        [
            $"ID сообщения: {message.Id}",
            $"Ключ диалога: {message.PeerKey}",
            $"Обновлено: {FormatTimestamp(message.UpdatedAtUnixMs)}",
            $"Статус доставки: {MapMessageStatus(message.Status)}",
        ];

        if (message.RemoteDeliveredAtUnixMs.HasValue)
        {
            lines.Add($"Подтверждение доставки: {FormatTimestamp(message.RemoteDeliveredAtUnixMs.Value)}");
        }

        if (securityDetails is not null)
        {
            lines.Add($"Шифрование сообщения: {MapEncryptionState(securityDetails.EncryptionState)}");
            lines.Add($"Тип защищённого сообщения: {MapPayloadKind(securityDetails.PayloadKind)}");
            lines.Add($"Сессия: {MapSessionState(securityDetails.BootstrapState)}");
            lines.Add($"Зашифрованные данные в хранилище: {(securityDetails.HasEncryptedPayload ? "есть" : "нет")}");
            lines.Add($"Удалённый аккаунт: {FormatOptional(securityDetails.RemoteAccountId)}");
            lines.Add($"Удалённое устройство: {FormatOptional(securityDetails.RemoteDeviceLabel ?? securityDetails.RemoteDeviceId)}");
            lines.Add($"Состояние доверия: {MapTrustState(linkedContact?.TrustState ?? securityDetails.TrustState ?? "unverified")}");
        }

        if (chatSecuritySummary is not null)
        {
            lines.Add($"Предупреждение безопасности: {MapSecurityWarning(chatSecuritySummary.Warning)}");
        }

        if (message.Attachment is not null)
        {
            bool isOutgoingAttachment = message.Direction.Equals("outgoing", StringComparison.OrdinalIgnoreCase);
            lines.Add($"Вложение: {message.Attachment.FileName}");
            lines.Add($"Размер файла: {FormatFileSize(message.Attachment.FileSizeBytes)}");
            lines.Add($"Режим защиты: {BuildAttachmentProtectionText(message.Attachment)}");
            lines.Add($"Статус вложения: {BuildAttachmentStatusText(message.Attachment, isOutgoingAttachment)}");
        }

        return string.Join('\n', lines);
    }

    public static string FormatFileSize(long bytes)
    {
        double value = bytes;
        string[] units = ["B", "KB", "MB", "GB", "TB"];
        int unitIndex = 0;

        while (value >= 1024d && unitIndex < units.Length - 1)
        {
            value /= 1024d;
            unitIndex++;
        }

        return unitIndex == 0
            ? $"{value:0} {units[unitIndex]}"
            : $"{value:0.0} {units[unitIndex]}";
    }

    // A file already on this disk shows no bar, whatever the transfer state says (a saved copy kept a
    // half-full bar under «Сохранено», seen 2026-10-13).
    internal static bool ShouldShowAttachmentProgress(AttachmentRecord attachment)
        => IsAttachmentPreparing(attachment) ||
           !attachment.LocalBlobAvailable && string.IsNullOrWhiteSpace(attachment.LocalSavedPath) &&
           (attachment.State.Equals("downloading", StringComparison.OrdinalIgnoreCase) ||
           attachment.State.Equals("requested", StringComparison.OrdinalIgnoreCase) ||
           attachment.State.Equals("waiting_for_reconnect", StringComparison.OrdinalIgnoreCase) ||
           attachment.State.Equals("verifying", StringComparison.OrdinalIgnoreCase));

    internal static double BuildAttachmentProgressValue(AttachmentRecord attachment)
    {
        if (attachment.FileSizeBytes <= 0)
        {
            return 0d;
        }

        double value = attachment.BytesTransferred * 100d / attachment.FileSizeBytes;
        return Math.Max(0d, Math.Min(100d, value));
    }

    /// Our file whose tree the core is still reading (a big one, in the background): not offered
    /// yet, nothing to save or revoke — only to cancel.
    internal static bool IsAttachmentPreparing(AttachmentRecord attachment)
        => attachment.State.Equals("preparing", StringComparison.OrdinalIgnoreCase);

    internal static bool CanDownloadAttachment(AttachmentRecord attachment)
        => !IsAttachmentPreparing(attachment) &&
           (!string.IsNullOrWhiteSpace(attachment.LocalSavedPath) ||
           attachment.LocalBlobAvailable ||
           (attachment.RemoteHolderAvailable &&
            !attachment.State.Equals("unavailable", StringComparison.OrdinalIgnoreCase) &&
            !attachment.State.Equals("requested", StringComparison.OrdinalIgnoreCase) &&
            !attachment.State.Equals("downloading", StringComparison.OrdinalIgnoreCase) &&
            !attachment.State.Equals("verifying", StringComparison.OrdinalIgnoreCase)));

    /// Only the one who offered a file takes it back. A copy we downloaded may be served on too (a
    /// group member hands it on, history sync hands it to our other device) — that is not ours to
    /// "revoke" from the sender.
    internal static bool CanRevokeAttachmentAccess(AttachmentRecord attachment)
        => attachment.LocalBlobAvailable &&
           attachment.LocalServingEnabled &&
           !attachment.State.Equals("downloaded", StringComparison.OrdinalIgnoreCase);

    // The pictures the core re-encodes as a photo (omni-storage photo.rs decodes these), and the
    // largest it takes (PHOTO_SOURCE_MAX_BYTES); anything else goes as a file.
    private static readonly string[] PhotoExtensions = [".jpg", ".jpeg", ".png", ".bmp", ".gif", ".webp"];
    internal const long PhotoSourceMaxBytes = 64L * 1024 * 1024;

    internal static bool CanSendAsPhoto(string? fileName, long fileSizeBytes)
        => fileSizeBytes is > 0 and <= PhotoSourceMaxBytes &&
           PhotoExtensions.Contains(Path.GetExtension(fileName ?? string.Empty), StringComparer.OrdinalIgnoreCase);

    // A photo's size in a bubble: within 300 x 340, the picture's own shape, at least 120 wide.
    internal const double PhotoBubbleMaxWidth = 300d;
    internal const double PhotoBubbleMaxHeight = 340d;

    internal static (double Width, double Height) FitPhotoInBubble(int width, int height)
    {
        if (width <= 0 || height <= 0)
        {
            return (PhotoBubbleMaxWidth, PhotoBubbleMaxWidth * 0.75);
        }

        double scale = Math.Min(PhotoBubbleMaxWidth / width, PhotoBubbleMaxHeight / height);
        double shownWidth = Math.Max(120d, Math.Round(width * scale));
        return (shownWidth, Math.Round(shownWidth * height / width));
    }

    internal static string BuildAttachmentProtectionText(AttachmentRecord attachment)
        => attachment.ContentEncryption switch
        {
            "chacha20_poly1305_v1" => "Сквозное шифрование",
            "e2ee_media" => "Сквозное шифрование",
            null or "" => "Режим защиты не указан",
            _ => $"Режим: {attachment.ContentEncryption}",
        };

    /// A group file: one a member downloaded is handed on to the others (core
    /// `serve_downloaded_attachment`) — say so, not the 1:1 «available at both ends».
    internal static string BuildGroupAttachmentStatusText(AttachmentRecord attachment, bool isMine)
        => !isMine &&
           attachment.State.Equals("downloaded", StringComparison.OrdinalIgnoreCase) &&
           attachment.LocalServingEnabled
            ? string.Empty
            : BuildAttachmentStatusText(attachment, isMine);

    // The bubble speaks only when there is something to do or to wait for: «Можно скачать», the
    // download's progress, «Ждём подключения», a failure, a revoked access. A file that is simply here
    // says nothing — «Сохранено локально» stood under every file of a chat (owner, 2026-10-13: UI/UX
    // pass). Empty = the line is hidden.
    internal static string BuildAttachmentStatusText(AttachmentRecord attachment, bool isOutgoing)
    {
        bool hasSavedCopy = !string.IsNullOrWhiteSpace(attachment.LocalSavedPath);

        bool downloaded = attachment.State.Equals("downloaded", StringComparison.OrdinalIgnoreCase);
        if (hasSavedCopy && (!attachment.LocalServingEnabled || downloaded))
        {
            return attachment.State.Equals("revoked", StringComparison.OrdinalIgnoreCase)
                ? "Отправитель закрыл доступ — ваша копия осталась"
                : string.Empty;
        }

        if (attachment.LocalBlobAvailable)
        {
            return attachment.State.Equals("revoked", StringComparison.OrdinalIgnoreCase)
                ? "Отправитель закрыл доступ — ваша копия осталась"
                : string.Empty;
        }

        return attachment.State.ToLowerInvariant() switch
        {
            "preparing" => $"Подготовка {BuildAttachmentProgressValue(attachment):0}%",
            "available" when attachment.RemoteHolderAvailable
                => "Можно скачать",
            "requested" => "Запрос отправлен",
            "downloading" when attachment.SourceCount >= 2
                => $"Загрузка {BuildAttachmentProgressValue(attachment):0}% · источников: {attachment.SourceCount}",
            "downloading" => $"Загрузка {BuildAttachmentProgressValue(attachment):0}%",
            "verifying" => "Проверка файла",
            "waiting_for_reconnect" => "Ждём подключения",
            "downloaded" => isOutgoing ? "Доставлен" : string.Empty,
            "revoked" => "Доступ закрыт",
            // The core's reason is for the logs: the bubble showed «abandoned by sender (TM-005
            // stale-transfer GC)» verbatim (seen 2026-10-10).
            "unavailable" => "Файл недоступен",
            "failed" when attachment.LastError == "cancelled" => "Отправка отменена",
            "failed" => isOutgoing ? "Не удалось передать" : "Передача прервалась",
            _ => "Вложение доступно",
        };
    }

    public static string MapChatSessionState(string? state)
        => (state ?? string.Empty).Trim().ToLowerInvariant() switch
        {
            "connected" => "подключена",
            "reconnecting" => "переподключаемся",
            "paused" => "завершена пользователем",
            "idle" => "ожидание",
            _ => "ожидание",
        };

    public static string BuildChatSessionPingText(ChatSessionSummary? summary)
        => summary?.LastRoundTripMs is long roundTripMs && roundTripMs >= 0
            ? $"Пинг: {roundTripMs} мс"
            : "Пинг: —";

    public static string BuildChatSessionKeepaliveText(ChatSessionSummary? summary, long nowUnixMs)
    {
        if (summary?.LastConfirmedKeepaliveAtUnixMs is not long lastConfirmedKeepaliveAtUnixMs)
        {
            return "Keepalive: —";
        }

        return $"Keepalive: {FormatRelativeAgeFromUnixMs(lastConfirmedKeepaliveAtUnixMs, nowUnixMs)}";
    }

    public static string BuildChatSessionNetworkLabel(
        ChatSessionSummary? summary,
        IReadOnlyList<NetworkInterfaceSnapshot> inventory)
    {
        string? localEndpoint = string.IsNullOrWhiteSpace(summary?.LocalEndpoint)
            ? null
            : summary.LocalEndpoint.Trim();
        string? peerEndpoint = string.IsNullOrWhiteSpace(summary?.PeerEndpoint)
            ? null
            : summary.PeerEndpoint.Trim();

        if (localEndpoint is null && peerEndpoint is null)
        {
            return "—";
        }

        string? localHost = localEndpoint is null ? null : TryExtractEndpointHost(localEndpoint);
        string? peerHost = peerEndpoint is null ? null : TryExtractEndpointHost(peerEndpoint);

        if (IsPublicIpv4InternetHost(peerHost))
        {
            return "Интернет";
        }

        if (TryBuildLoopbackNetworkLabel(localHost, peerHost) is string loopbackLabel)
        {
            return loopbackLabel;
        }

        if (TryFindInventoryMatch(localHost, peerHost, inventory) is NetworkInterfaceSnapshot match)
        {
            return BuildNetworkInterfaceLabel(match);
        }

        if (IsIpv6Host(peerHost))
        {
            return "IPv6";
        }

        // ЛОКАЛЬНЫЙ адрес — законный последний ответ: он говорит, ЧЕРЕЗ ЧТО мы вышли, и больше
        // его на экране нигде нет. А вот адрес ПИРА повторять сюда нельзя: он уже стоит в
        // соседнем поле «Endpoint». Здесь стояло `localEndpoint ?? peerEndpoint`, и на живом
        // экране 2026-08-31 вторая половина дала «Сеть: d43dcaae…444 • Endpoint: d43dcaae…444» —
        // одно значение в двух полях подряд, причём не сеть, а адрес УЗЛА (под mesh в
        // `PeerEndpoint` приезжал он, ни одна ветка разбора его не узнавала, и всё сваливалось
        // сюда). Адрес узла в summary чинит ядро; дублирование — вот эта строка.
        return localEndpoint ?? "—";
    }

    /// <summary>
    /// Which of the contact's devices answers, and where it is: a contact with a laptop and a PC is one
    /// chat, and «Пинг 4 мс» read as the far PC when it was the laptop on this very computer (stand,
    /// 2026-10-14). Names of a contact's devices are not known (only ids), so: how many, and where.
    /// </summary>
    public static string? BuildChatSessionPeerDeviceText(
        ChatSessionSummary? summary,
        IReadOnlyList<NetworkInterfaceSnapshot> inventory)
    {
        if (summary?.PeerDeviceCount is not int count || count == 0)
        {
            return null;
        }

        string? peerHost = string.IsNullOrWhiteSpace(summary.PeerEndpoint)
            ? null
            : TryExtractEndpointHost(summary.PeerEndpoint.Trim());
        string? where = peerHost is null
            ? null
            : TryBuildLoopbackNetworkLabel(null, peerHost) is not null ||
              inventory.Any(network => NetworkHasAddress(network, peerHost))
                ? "на этом же компьютере"
                : IsPublicIpv4InternetHost(peerHost) || IsIpv6Host(peerHost)
                    ? "через интернет"
                    : "в локальной сети";
        string which = count > 1 ? $"одно из {count}" : "единственное";
        return where is null ? $"Устройство собеседника: {which}" : $"Устройство собеседника: {which}, {where}";
    }

    public static string BuildChatSessionEndpointText(ChatSessionSummary? summary)
        => string.IsNullOrWhiteSpace(summary?.PeerEndpoint)
            ? "Endpoint: —"
            : $"Endpoint: {NormalizeEndpointForDisplay(summary.PeerEndpoint)}";

    public static string FormatRelativeAgeFromUnixMs(long? unixMs, long nowUnixMs)
    {
        if (unixMs is not long value)
        {
            return "—";
        }

        long ageMs = Math.Max(0, nowUnixMs - value);
        return FormatAge(ageMs);
    }
}


