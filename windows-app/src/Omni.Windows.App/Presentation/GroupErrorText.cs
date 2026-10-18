namespace Omni.Windows.App.Presentation;

/// <summary>
/// What a refused group action means, in the user's words. The core decides (rights are checked
/// before a Commit is merged) and says why in a stable English phrase; the shell used to drop that
/// phrase and show a generic "invalid input". Unknown phrases are shown as they are — an honest
/// reason beats a vague one.
/// </summary>
internal static class GroupErrorText
{
    /// An over-long message or caption (16 KB, `CHAT_BODY_MAX_BYTES` / `GROUP_TEXT_MAX_BYTES`).
    internal const string TooLongMessage =
        "Сообщение слишком длинное — больше 16 КБ (около 8 тысяч букв кириллицей). Отправьте текст файлом.";

    private static readonly (string Core, string User)[] Known =
    {
        ("only an admin may do this", "Это может только админ группы."),
        ("a group keeps at least one admin", "В группе должен остаться хотя бы один админ."),
        ("the last admin must name another admin before leaving", "Вы последний админ — назначьте другого админа, прежде чем выйти."),
        ("not an active member of the group", "Вы больше не участник этой группы."),
        ("unknown group", "Группа не найдена — возможно, вы из неё вышли."),
        ("no such member", "Этого участника уже нет в группе."),
        ("no such invitation", "Приглашение больше не действует."),
        ("no call is going on in this group", "В группе сейчас нет звонка."),
        // Before the empty-body line: both say "message body".
        ("byte limit", TooLongMessage),
        ("caption is too long", TooLongMessage),
        ("message body must not be empty", "Введите сообщение."),
        ("group name must be 1..128 bytes", "Название группы пустое или слишком длинное — до 64 букв, эмодзи занимают больше места."),
        ("group control needs a known contact", "Пригласить можно только контакт из вашего списка."),
        ("contact has no remote account for group control", "Этот контакт ещё не подтверждён — пригласить его пока нельзя."),
    };

    internal static string ForCoreMessage(string? coreMessage)
    {
        string message = coreMessage?.Trim() ?? string.Empty;
        foreach ((string core, string user) in Known)
        {
            if (message.Contains(core, StringComparison.OrdinalIgnoreCase))
            {
                return user;
            }
        }

        return message.Length == 0 ? "Не удалось выполнить действие с группой." : $"Группа: {message}";
    }
}
