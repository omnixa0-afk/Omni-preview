namespace Omni.Windows.App.Presentation;

/// <summary>
/// The restore-by-phrase screen in the person's words. The core's check of the phrase came to the
/// screen as its own English summary («This local profile is already known on this device…» with
/// profile and account ids), and a mistyped phrase as «Некорректные входные данные» (QA 2026-10-17).
/// </summary>
internal static class RecoveryText
{
    /// <summary>What the check of the phrase found, by its conflict code.</summary>
    internal static string ForPreview(string? conflict, bool canRestore) => conflict switch
    {
        "existing_profile_already_present" =>
            "Этот профиль уже есть на компьютере — откройте его из списка профилей.",
        "profile_locked" =>
            "Этот профиль уже есть на компьютере и заперт — войдите в него паролем или фразой.",
        "profile_deleting" => "Этот профиль сейчас удаляется — попробуйте позже.",
        "profile_deleted" => "Этот профиль был удалён с компьютера.",
        "invalid_mnemonic" => InvalidPhrase,
        _ => canRestore ? "Фраза подходит — профиль будет восстановлен." : "Восстановить по этой фразе сейчас нельзя.",
    };

    /// <summary>A core reason about the phrase itself, or null when it is about something else.</summary>
    internal static string? ForCoreError(string? message)
    {
        if (string.IsNullOrEmpty(message))
        {
            return null;
        }

        if (message.Contains("must contain exactly", StringComparison.OrdinalIgnoreCase))
        {
            return "Во фразе должно быть 24 английских слова через пробел.";
        }

        if (message.Contains("invalid recovery phrase", StringComparison.OrdinalIgnoreCase) ||
            message.Contains("recovery phrase must not be empty", StringComparison.OrdinalIgnoreCase))
        {
            return InvalidPhrase;
        }

        return null;
    }

    private const string InvalidPhrase = "Фраза не подходит — проверьте слова и их порядок.";
}
