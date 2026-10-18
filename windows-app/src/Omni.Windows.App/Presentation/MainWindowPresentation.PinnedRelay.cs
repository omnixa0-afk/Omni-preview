using Microsoft.UI.Xaml.Controls;

namespace Omni.Windows.App.Presentation;

// The banner over «Публичные релеи» while the profile goes through a pinned relay
// (docs/decisions/public-relay-directory.md, stage 3): is it holding, and what happens while it is not.
internal static partial class MainWindowPresentation
{
    public static (InfoBarSeverity Severity, string Message) DescribePinnedRelay(
        bool linked,
        long? lostSecs,
        bool keepAddressHidden,
        bool always)
    {
        if (linked)
        {
            return (InfoBarSeverity.Success,
                "Связь идёт только через него, напрямую — нет: собеседники не видят ваш адрес.");
        }

        if (lostSecs is not { } secs)
        {
            return (InfoBarSeverity.Informational, "Подключаюсь к релею…");
        }

        string since = secs < 60 ? "меньше минуты" : secs < 3600 ? $"{secs / 60} мин" : $"{secs / 3600} ч";
        string back = always
            ? " Пробую снова и ищу его в каталоге — вернусь, как только он появится."
            : " Пробую снова — вернусь, как только он ответит.";
        return keepAddressHidden
            ? (InfoBarSeverity.Warning,
                $"Релей не отвечает {since}. Адрес не открываю: сообщения подождут, звонков пока не будет.{back}")
            : (InfoBarSeverity.Warning,
                $"Релей не отвечает {since}. Пока его нет, связь идёт как обычно — и напрямую тоже.{back}");
    }
}
