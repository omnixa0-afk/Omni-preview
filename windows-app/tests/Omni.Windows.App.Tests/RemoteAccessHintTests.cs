using Omni.Windows.App;
using Omni.Windows.App.Core;
using Xunit;

namespace Omni.Windows.App.Tests;

/// The contact-card hint under the "Удалённый доступ" toggle (remote-access-for-contacts.md). It
/// must steer an unverified contact to verification first, and name the mode plainly when granted.
public sealed class RemoteAccessHintTests
{
    private static ContactRemoteAccessRecord Granted(string? mode) => new("c", mode, null, null, null, null);

    [Fact]
    public void An_unverified_contact_without_a_grant_is_pointed_to_verification()
    {
        string hint = MainWindow.BuildRemoteAccessHint(verified: false, access: null);
        Assert.Contains("проверьте контакт", hint);
    }

    [Fact]
    public void Each_granted_mode_says_plainly_what_it_means()
    {
        Assert.Contains("без вопроса", MainWindow.BuildRemoteAccessHint(verified: true, Granted("unattended")));
        Assert.Contains("подтвердить", MainWindow.BuildRemoteAccessHint(verified: true, Granted("ask")));

        // Negative half: verified but not granted says it is off, not the verification nudge.
        string off = MainWindow.BuildRemoteAccessHint(verified: true, Granted(null));
        Assert.Contains("Выключено", off);
        Assert.DoesNotContain("проверьте контакт", off);
    }
    [Fact]
    public void The_grant_warning_says_what_is_given_and_what_is_not()
    {
        MainWindow.RemoteAccessWarning ask = MainWindow.BuildRemoteAccessWarning("Аня", unattended: false);
        string text = string.Join(" ", ask.Paragraphs);
        Assert.Contains("весь экран", text);
        Assert.Contains("экран блокировки", text);
        Assert.Contains("Отозвать у всех", text);
        Assert.Contains("подтвердить", text);
        // Negative half: asking each time needs no extra acknowledgement.
        Assert.Null(ask.Acknowledgement);
    }

    [Fact]
    public void Without_a_question_needs_an_explicit_acknowledgement()
    {
        MainWindow.RemoteAccessWarning unattended = MainWindow.BuildRemoteAccessWarning("Аня", unattended: true);
        Assert.Contains("без вопроса", unattended.Title);
        Assert.NotNull(unattended.Acknowledgement);
        Assert.Contains("когда меня нет рядом", unattended.Acknowledgement);
        Assert.DoesNotContain(unattended.Paragraphs, p => p.Contains("подтвердить"));
    }
    [Fact]
    public void The_other_direction_is_shown_only_when_given()
    {
        string? online = MainWindow.BuildRemoteAccessReceivedText("ПК", "unattended", online: true);
        Assert.Contains("разрешил вам управлять", online);
        Assert.Contains("без вопроса", online);
        Assert.Contains("не в сети", MainWindow.BuildRemoteAccessReceivedText("ПК", "ask", online: false));
        // Negative half: nothing given, nothing said.
        Assert.Null(MainWindow.BuildRemoteAccessReceivedText("ПК", null, online: true));
    }

    [Fact]
    public void The_last_session_reads_like_a_person_would_say_it()
    {
        var now = new System.DateTimeOffset(2026, 10, 5, 23, 0, 0, System.TimeSpan.FromHours(3));
        long today = now.AddHours(-1).ToUnixTimeMilliseconds();
        long yesterday = now.AddDays(-1).ToUnixTimeMilliseconds();
        Assert.Equal("Последний сеанс: сегодня в 22:00", MainWindow.BuildLastSessionText(today, now));
        Assert.StartsWith("Последний сеанс: вчера", MainWindow.BuildLastSessionText(yesterday, now));
        Assert.Null(MainWindow.BuildLastSessionText(null, now));
    }
}
