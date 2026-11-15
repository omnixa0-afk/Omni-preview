namespace Omni.Windows.App.Presentation;

/// <summary>
/// What a refused workspace action means, in the user's words. The core says why in a stable English
/// phrase (rights, a folder, a version that has not arrived); the shell used to show all of them as
/// «Некорректные входные данные». Unknown phrases are shown as they are, like <see cref="GroupErrorText"/>.
/// </summary>
internal static class WorkspaceErrorText
{
    private static readonly (string Core, string User)[] Known =
    {
        ("wait for the file tree before accepting custody", "Дерево файлов проекта ещё не пришло, и его размер неизвестен. Подождите, пока оно придёт от участников, и согласитесь снова."),
        ("file tree has not arrived yet", "Дерево файлов ещё не пришло от участников — попробуйте чуть позже."),
        ("repository has not arrived yet", "Проект ещё не пришёл от участников — попробуйте чуть позже."),
        ("the proposed version has not arrived yet", "Эта версия ещё не пришла — попробуйте чуть позже."),
        ("this nomination is no longer active", "Это предложение стать хранителем уже отменено."),
        ("no custodian nomination", "Вас не предлагали в хранители."),
        ("choose a folder before accepting custody", "Выберите папку для хранения проекта."),
        ("choose an absolute custody folder", "Выберите папку для хранения целиком, с диском (например, D:\\Omni)."),
        ("choose an absolute workspace root folder", "Выберите папку целиком, с диском (например, D:\\Omni)."),
        ("choose an absolute project folder", "Выберите папку проекта целиком, с диском."),
        ("choose a project folder", "Выберите папку проекта."),
        ("choose a destination file", "Выберите, куда сохранить файл."),
        ("destination changed", "Файл в выбранном месте изменился — выберите место заново."),
        ("download was cancelled", "Загрузка отменена."),
        ("file content has not been accepted", "Содержимое этой версии ещё не принято — скачать его могут автор и хранители."),
        ("file is not in the version", "Этого файла нет в версии."),
        ("this version is not available for review", "Эта версия недоступна для принятия."),
        ("only an admin appoints custodians", "Назначать хранителей может только админ."),
        ("only an admin dismisses custodians", "Снимать хранителей может только админ."),
        ("only an admin changes file policy", "Правила проекта меняет только админ."),
        ("workspace role does not permit this action", "Ваша роль в workspace этого не позволяет."),
        ("the repository has no folder here", "У этого проекта здесь нет папки — сначала «Забрать сюда»."),
        ("the repository holds code", "Это проект с кодом: версии сохраняются в git."),
        ("the folder was updated meanwhile", "Папка проекта обновлялась в это время — повторите."),
        ("our line holds nothing the shared version does not", "Возвращать нечего: в папке уже общая версия."),
        ("our line is accepted; it joins the shared version", "Твою версию уже приняли — она становится общей."),
        ("the folder is being updated","Папка проекта сейчас обновляется — повторите через минуту."),
        ("this device keeps the project's history", "Этот компьютер хранит историю проекта — сначала откажитесь от хранения в «Хранителях и правилах»."),
        ("only a files project folder can be removed", "Убрать так можно только папку проекта с файлами."),
        ("only an admin or a custodian releases a revision", "Выпускать ревизии могут админ и хранители проекта."),
        ("only an admin or a custodian approves a revision", "Одобрять ревизии могут админ и хранители проекта."),
        ("a revision is approved by someone other than who released it", "Ревизию одобряет не тот, кто её выпустил: другой хранитель или админ."),
        ("this version is not a revision", "Эта версия — не ревизия."),
        ("only an admin turns tasks on", "Включает задачи админ workspace."),
        ("tasks are not turned on in this workspace", "Задачи в этом workspace ещё не включены."),
        ("a task needs a title", "У задачи должно быть название — до 200 знаков."),
        ("no such task", "Такой задачи нет — возможно, её ещё не получили отсюда."),
        ("a task is given to a member of the workspace", "Задачу можно дать только участнику workspace."),
        ("several members have the name", "Участников с таким именем несколько. Наберите @ и выберите нужного из списка."),
        ("a comment is some text", "Комментарий — непустой текст до 16 КБ."),
        ("the task's description is too long", "Описание задачи слишком длинное (до 64 КБ)."),
        ("only an admin or a custodian keeps a version for good", "Хранить версию навсегда могут отметить админ и хранители проекта."),
        ("this revision designation is taken", "Такое обозначение уже есть у другой ревизии — выберите другое."),
        ("the version is not an accepted one", "Эта версия не принята в проект."),
        ("the current version already has these files", "В текущей версии уже такие же файлы — восстанавливать нечего."),
        ("invalid revision", "Обозначение ревизии — до 16 символов, комментарий — до 1000."),
        ("only an admin sets how long versions are kept", "Правило хранения версий задаёт админ."),
        ("the version's content was thinned out", "Содержимое этой версии не сохранено (прорежено) — восстановить её нельзя."),
        ("invalid retention rule", "Хранить нужно хотя бы последнюю версию; не больше 100 000 версий или дней. Маска — имя файла без папки (*.stl), до 64 символов, не больше 32 масок."),
        ("choose a folder outside Omni's own data", "Выберите папку вне данных Omni."),
        ("not an active workspace member", "Вы больше не участник этого workspace."),
        ("not an active member of the group", "Вы больше не участник этого workspace."),
        ("unknown workspace", "Workspace не найден — возможно, вы из него вышли."),
        ("no such repository in the workspace", "Такого проекта в workspace нет."),
        ("take the project into a folder first", "Сначала заберите проект в папку: в работу берут файл из своей папки."),
        ("choose the files to take", "Выберите файлы, которые берёте в работу."),
        ("only an admin gives back someone else's file", "Снять чужой замок может только админ."),
        ("the folder holds no version yet", "В папке ещё нет версии проекта."),
        ("invalid file lock", "Файл берут в работу не больше чем на неделю."),
    };

    // «<path>: <why>» — the file first, then what about it.
    private static readonly (string Core, string User)[] AboutFile =
    {
        (": the folder does not hold its latest version yet", "В папке ещё не последняя версия {0} — дождитесь обновления папки (закройте файл, если он открыт) и возьмите снова."),
        (": not in the shared version", "{0} нет в общей версии — сначала сохраните версию."),
        (": not in the folder's version", "{0} нет в версии папки."),
        (": not taken", "{0} сейчас не в работе."),
        (": nothing changed here", "{0} у вас не изменён — откладывать нечего."),
    };

    internal static string ForCoreMessage(string? coreMessage)
    {
        string message = coreMessage?.Trim() ?? string.Empty;
        const string kind = "invalid input: ";
        if (message.StartsWith(kind, StringComparison.OrdinalIgnoreCase)) message = message[kind.Length..];
        int badName = message.IndexOf(": the file name cannot be written on every system", StringComparison.Ordinal);
        if (badName > 0)
        {
            return $"Имя файла {message[..badName]} нельзя записать на всех компьютерах — переименуйте его и сохраните снова.";
        }

        int taken = message.IndexOf(": taken by ", StringComparison.Ordinal);
        int until = message.LastIndexOf(" until ", StringComparison.Ordinal);
        if (taken > 0 && until > taken && long.TryParse(message[(until + 7)..], out long untilMs))
        {
            string holder = message[(taken + 11)..until];
            return $"{message[..taken]} в работе у {holder} до {MainWindowPresentation.FormatMessageTime(untilMs)}. Попросите отдать или дождитесь срока.";
        }

        foreach ((string core, string user) in AboutFile)
        {
            int at = message.IndexOf(core, StringComparison.Ordinal);
            if (at > 0)
            {
                return string.Format(System.Globalization.CultureInfo.InvariantCulture, user, message[..at]);
            }
        }

        int open = message.IndexOf(": the file is open in a program", StringComparison.Ordinal);
        if (open > 0)
        {
            return $"Файл {message[..open]} открыт в программе — закройте его и сохраните снова.";
        }

        foreach ((string core, string user) in Known)
        {
            if (message.Contains(core, StringComparison.OrdinalIgnoreCase))
            {
                return user;
            }
        }

        return message.Length == 0 ? "Не удалось выполнить действие с проектом." : $"Workspace: {message}";
    }
}
