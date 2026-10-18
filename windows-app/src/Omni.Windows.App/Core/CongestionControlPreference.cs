namespace Omni.Windows.App.Core;

/// <summary>
/// Ручной выбор контроллера перегрузки, сохраняемый на устройстве.
///
/// Зачем он есть: сравнить контроллеры честно можно только A/B на ОДНОЙ живой сборке — канал и
/// сота дрейфуют вдвое за час, прогоны разных сборок несравнимы. До этого переключателя для A/B
/// требовалась переменная окружения, то есть перезапуск процесса.
///
/// Само правило и оба замера, на которых оно стоит, живут в ядре
/// (`select_congestion_control`, docs/bugs/cubic-caps-this-path-at-four-percent.md и
/// uplink-goodput-retransmit.md §P1). Здесь только хранение и показ.
/// </summary>
internal static class CongestionControlPreference
{
    internal const string Auto = "auto";
    internal const string Bbr = "bbr";
    internal const string Cubic = "cubic";
    internal const string NewReno = "newreno";

    /// <summary>
    /// Привести сохранённое значение к тому, что ядро точно примет. Мусор и пустое — «авто»:
    /// испорченный файл настроек не должен запирать пользователя в контроллере, которого он не
    /// выбирал. Проверку самого значения делает ядро, здесь только нормализация хранилища.
    /// </summary>
    internal static string Normalize(string? raw)
        => (raw ?? string.Empty).Trim().ToLowerInvariant() switch
        {
            Bbr => Bbr,
            Cubic => Cubic,
            NewReno => NewReno,
            _ => Auto,
        };

    /// <summary>Человеческое имя действующего контроллера для строки состояния.</summary>
    internal static string DisplayName(string? mode)
        => Normalize(mode) switch
        {
            Bbr => "BBR",
            Cubic => "CUBIC",
            NewReno => "NewReno",
            _ => "авто",
        };

    /// <summary>
    /// Строка состояния под переключателем. На «авто» она обязана назвать реальный итог правила —
    /// иначе настройка молчит ровно там, где решение принимает не пользователь.
    /// </summary>
    internal static string StatusText(string? selected, string? effective)
    {
        string chosen = Normalize(selected);
        string applied = Normalize(effective);
        string appliedName = DisplayName(applied);

        if (chosen == Auto)
        {
            return $"Сейчас действует: {appliedName} (выбрано по стоимости канала)";
        }

        // Переменная окружения стоит выше переключателя нарочно (её ставит скрипт A/B под
        // конкретный прогон). Молчать об этом нельзя: иначе настройка показывала бы одно, а
        // соединение ехало на другом.
        return applied == chosen
            ? $"Сейчас действует: {appliedName}"
            : $"Сейчас действует: {appliedName} — переключатель перебит переменной OMNI_QUIC_CC";
    }
}
