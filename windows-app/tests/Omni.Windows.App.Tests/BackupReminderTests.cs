using System;
using Omni.Windows.App.Core;
using Xunit;

namespace Omni.Windows.App.Tests;

/// <summary>
/// Строка о резервной копии: никогда — сказать, что без неё; свежая — когда; старше месяца — напомнить.
/// </summary>
public sealed class BackupReminderTests
{
    private static readonly DateTimeOffset Now = new(2026, 10, 12, 12, 0, 0, TimeSpan.Zero);

    [Fact]
    public void Never_Saved_Says_What_Is_At_Stake()
    {
        Assert.Contains("ещё не сохранялась", BackupReminder.Describe(null, Now));
    }

    [Fact]
    public void A_Fresh_Copy_Shows_Its_Date_Without_A_Reminder()
    {
        string text = BackupReminder.Describe(Now.AddDays(-3).ToUnixTimeMilliseconds(), Now);
        Assert.StartsWith("Последняя копия: ", text);
        Assert.DoesNotContain("сохраните новую", text);
    }

    [Fact]
    public void A_Copy_Older_Than_A_Month_Asks_For_A_New_One()
    {
        Assert.Contains("сохраните новую", BackupReminder.Describe(Now.AddDays(-31).ToUnixTimeMilliseconds(), Now));
    }
}
