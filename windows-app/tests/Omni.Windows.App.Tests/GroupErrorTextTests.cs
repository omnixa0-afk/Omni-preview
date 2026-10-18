using Omni.Windows.App.Presentation;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class GroupErrorTextTests
{
    // The core's reason reaches the user in their words (it used to become "invalid input").
    [Theory]
    [InlineData("invalid input: only an admin may do this", "Это может только админ группы.")]
    [InlineData("the last admin must name another admin before leaving", "Вы последний админ — назначьте другого админа, прежде чем выйти.")]
    [InlineData("no call is going on in this group", "В группе сейчас нет звонка.")]
    public void KnownCoreReasonsReadAsSentences(string core, string user)
        => Assert.Equal(user, GroupErrorText.ForCoreMessage(core));

    // An over-long message is not "enter a message" — both reasons say "message body".
    [Fact]
    public void AnOverLongMessageIsNotReadAsAnEmptyOne()
    {
        Assert.Equal(
            GroupErrorText.TooLongMessage,
            GroupErrorText.ForCoreMessage("message body exceeds the 16384 byte limit; send large content as an attachment instead"));
        Assert.Equal("Введите сообщение.", GroupErrorText.ForCoreMessage("message body must not be empty"));
    }

    // Negative half: an unknown reason is shown as it is, never swallowed into a generic text.
    [Fact]
    public void UnknownReasonIsShownNotSwallowed()
    {
        Assert.Equal("Группа: welcome is for another group", GroupErrorText.ForCoreMessage("welcome is for another group"));
        Assert.Equal("Не удалось выполнить действие с группой.", GroupErrorText.ForCoreMessage(null));
    }
}
