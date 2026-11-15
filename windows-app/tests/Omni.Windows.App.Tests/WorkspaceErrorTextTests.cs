using Omni.Windows.App.Presentation;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class WorkspaceErrorTextTests
{
    // The core's reason reaches the user (it used to become «Некорректные входные данные»).
    [Theory]
    [InlineData("invalid input: wait for the file tree before accepting custody: its size is not known yet", "Дерево файлов проекта ещё не пришло, и его размер неизвестен. Подождите, пока оно придёт от участников, и согласитесь снова.")]
    [InlineData("only an admin appoints custodians", "Назначать хранителей может только админ.")]
    [InlineData("invalid input: this device keeps the project's history; give up keeping it first", "Этот компьютер хранит историю проекта — сначала откажитесь от хранения в «Хранителях и правилах».")]
    [InlineData("invalid input: models/корпус.prt: the file is open in a program — close it and save again", "Файл models/корпус.prt открыт в программе — закройте его и сохраните снова.")]
    [InlineData("invalid input: cad/корпус.prt: the folder does not hold its latest version yet", "В папке ещё не последняя версия cad/корпус.prt — дождитесь обновления папки (закройте файл, если он открыт) и возьмите снова.")]
    [InlineData("invalid input: a.prt: not taken", "a.prt сейчас не в работе.")]
    public void KnownCoreReasonsReadAsSentences(string core, string user)
        => Assert.Equal(user, WorkspaceErrorText.ForCoreMessage(core));

    // «Беру в работу» someone else holds: who, and until when.
    [Fact]
    public void AFileTakenBySomeoneElseNamesTheHolder()
        => Assert.StartsWith("cad/a.prt в работе у Иван Петров до ", WorkspaceErrorText.ForCoreMessage("invalid input: cad/a.prt: taken by Иван Петров until 1793520273294"));

    [Fact]
    public void AnUnknownReasonIsShownAsItIs()
        => Assert.Equal("Workspace: something new", WorkspaceErrorText.ForCoreMessage("something new"));
}
