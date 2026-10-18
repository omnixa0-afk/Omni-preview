using Omni.Windows.App.Presentation;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class RecoveryTextTests
{
    // The core's English summary («This local profile is already known…») never reaches the screen.
    [Theory]
    [InlineData("existing_profile_already_present", false, "Этот профиль уже есть на компьютере — откройте его из списка профилей.")]
    [InlineData(null, true, "Фраза подходит — профиль будет восстановлен.")]
    public void ThePreviewReadsAsASentence(string? conflict, bool canRestore, string expected)
        => Assert.Equal(expected, RecoveryText.ForPreview(conflict, canRestore));

    // A mistyped phrase is named as such, not «Некорректные входные данные»; other reasons pass by.
    [Fact]
    public void APhraseErrorIsNamedAndOthersAreLeftAlone()
    {
        Assert.Equal(
            "Фраза не подходит — проверьте слова и их порядок.",
            RecoveryText.ForCoreError("invalid recovery phrase: checksum or word format check failed (x)"));
        Assert.Equal(
            "Во фразе должно быть 24 английских слова через пробел.",
            RecoveryText.ForCoreError("recovery phrase must contain exactly 24 English words"));
        Assert.Null(RecoveryText.ForCoreError("password does not unlock this profile"));
    }
}
