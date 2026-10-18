using System;

namespace Omni.Windows.App.Core;

/// <summary>
/// Строка о резервной копии в настройках (docs/decisions/account-recovery-from-friends.md, Г):
/// когда была последняя и не пора ли новая. Через месяц — напоминание: файл копии — единственный
/// путь к полной сохранности без чужой помощи.
/// </summary>
internal static class BackupReminder
{
    public static readonly TimeSpan RemindAfter = TimeSpan.FromDays(30);

    public static string Describe(long? lastBackupAtUnixMs, DateTimeOffset now)
    {
        if (lastBackupAtUnixMs is not long at)
        {
            return "Резервная копия ещё не сохранялась. Без неё переписка восстановится только со слов собеседников.";
        }

        DateTimeOffset when = DateTimeOffset.FromUnixTimeMilliseconds(at).ToLocalTime();
        string text = $"Последняя копия: {when:dd.MM.yyyy HH:mm}.";
        return now - when > RemindAfter ? $"{text} Больше месяца назад — сохраните новую." : text;
    }
}
