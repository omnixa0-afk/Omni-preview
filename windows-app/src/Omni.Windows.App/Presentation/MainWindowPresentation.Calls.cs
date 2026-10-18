using Omni.Windows.App.Core;

namespace Omni.Windows.App.Presentation;

internal static partial class MainWindowPresentation
{
    public static string MapCallState(string state, string direction)
        => (state ?? string.Empty).Trim().ToLowerInvariant() switch
        {
            "calling" => "Звоним",
            "ringing" when string.Equals(direction, "incoming", StringComparison.OrdinalIgnoreCase) => "Входящий звонок",
            "ringing" => "Ждём ответа",
            "connecting" => "Подключаем медиа",
            "active" => "Разговор идёт",
            "reconnecting" => "Соединение восстанавливается",
            "ended" => "Звонок завершён",
            _ => "Состояние звонка неизвестно",
        };

    public static string MapCallOutcome(string? outcome)
        => (outcome ?? string.Empty).Trim().ToLowerInvariant() switch
        {
            "completed" => "Завершён",
            "declined" => "Отклонён",
            "busy" => "Занято",
            "missed" => "Пропущен",
            "cancelled" => "Отменён",
            "connection_lost" => "Соединение потеряно",
            "failed" => "Ошибка звонка",
            _ => "Без итога",
        };

    public static string MapCallDeclineReason(string? reason)
        => (reason ?? string.Empty).Trim().ToLowerInvariant() switch
        {
            "declined" => "Отклонён",
            "busy" => "Занято",
            "not_available" => "Недоступен",
            "security_blocked" => "Заблокирован защитой",
            "already_in_call" => "Уже в другом звонке",
            _ => string.Empty,
        };

    public static string MapCallNetworkQuality(string? quality)
        => (quality ?? string.Empty).Trim().ToLowerInvariant() switch
        {
            "good" => "Сеть: хорошая",
            "fair" => "Сеть: средняя",
            "poor" => "Сеть: слабая",
            "recovering" => "Сеть: восстанавливается",
            _ => "Сеть: —",
        };

    public static string BuildCallPartySummary(IReadOnlyList<CallParticipantSummary> participants)
    {
        List<string> remoteLabels = participants
            .Where(participant => !participant.IsLocal)
            .Select(participant =>
                !string.IsNullOrWhiteSpace(participant.DisplayName)
                    ? participant.DisplayName!
                    : participant.AccountId)
            .Distinct(StringComparer.OrdinalIgnoreCase)
            .ToList();
        return remoteLabels.Count == 0
            ? "Собеседник"
            : string.Join(", ", remoteLabels);
    }

    public static string BuildCallDurationText(long startedAtUnixMs, long? connectedAtUnixMs, long? endedAtUnixMs)
    {
        long baselineUnixMs = connectedAtUnixMs ?? startedAtUnixMs;
        long finishUnixMs = endedAtUnixMs ?? DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
        if (finishUnixMs < baselineUnixMs)
        {
            finishUnixMs = baselineUnixMs;
        }

        return FormatDuration(TimeSpan.FromMilliseconds(finishUnixMs - baselineUnixMs));
    }

    public static string BuildCallHistorySummary(CallHistoryEvent history)
    {
        if (history.RemoteControl)
        {
            return BuildRemoteControlHistorySummary(history);
        }

        // A 1:1 chat line: the chat is already WITH the peer, so the line says what kind of call it was,
        // not who — "PC • Завершён • 00:38" read, under the "Вы:" of the preview, as "Вы PC Завершён"
        // (seen live 2026-10-10).
        bool outgoing = string.Equals(history.Direction, "outgoing", StringComparison.OrdinalIgnoreCase);
        string stateText = (history.Outcome ?? string.Empty).Trim().ToLowerInvariant() switch
        {
            "missed" => "Пропущенный звонок",
            "cancelled" => "Отменённый звонок",
            "declined" => "Отклонённый звонок",
            "busy" => outgoing ? "Исходящий звонок • Занято" : "Входящий звонок • Занято",
            "connection_lost" => (outgoing ? "Исходящий звонок" : "Входящий звонок") + " • связь прервалась",
            "failed" => "Звонок не удался",
            _ => outgoing ? "Исходящий звонок" : "Входящий звонок",
        };
        string declineText = MapCallDeclineReason(history.DeclineReason);
        string durationText = history.DurationSeconds.HasValue && history.ConnectedAtUnixMs.HasValue
            ? FormatDuration(TimeSpan.FromSeconds(history.DurationSeconds.Value))
            : string.Empty;
        List<string> parts = [stateText];
        if (!string.IsNullOrWhiteSpace(declineText) &&
            !stateText.Contains(declineText, StringComparison.OrdinalIgnoreCase) &&
            !string.Equals(history.Outcome, "declined", StringComparison.OrdinalIgnoreCase))
        {
            parts.Add(declineText);
        }
        if (!string.IsNullOrWhiteSpace(durationText))
        {
            parts.Add(durationText);
        }

        return string.Join(" • ", parts);
    }

    /// The chat line of a remote-control session (docs/decisions/remote-access-for-contacts.md):
    /// "Сеанс управления 14:02–14:20" when it ran, otherwise how it ended. The bubble's side says who
    /// controlled whom (outgoing — we controlled the peer).
    public static string BuildRemoteControlHistorySummary(CallHistoryEvent history)
    {
        if (history.ConnectedAtUnixMs is long connected && history.EndedAtUnixMs is long ended)
        {
            string from = DateTimeOffset.FromUnixTimeMilliseconds(connected).ToLocalTime().ToString("HH:mm");
            string to = DateTimeOffset.FromUnixTimeMilliseconds(ended).ToLocalTime().ToString("HH:mm");
            return $"Сеанс управления {from}–{to}";
        }

        bool weAsked = string.Equals(history.Direction, "outgoing", StringComparison.OrdinalIgnoreCase);
        string outcome = history.DeclineReason == "security_blocked"
            ? "доступ не выдан"
            : history.Outcome switch
            {
                // Who asked reads it differently: the asker got no answer; the owner missed a request.
                "missed" or "timeout" => weAsked ? "нет ответа" : "запрос пропущен",
                "declined" => weAsked ? "отклонён" : "вы отклонили",
                "cancelled" => weAsked ? "отменён" : "отменён собеседником",
                _ => MapCallOutcome(history.Outcome).ToLowerInvariant(),
            };
        return $"Сеанс управления: {outcome}";
    }
}


