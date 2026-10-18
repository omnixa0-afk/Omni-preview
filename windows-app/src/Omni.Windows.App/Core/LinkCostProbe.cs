using System;

namespace Omni.Windows.App.Core;

/// <summary>
/// Считают ли трафик пользователя на текущем канале. Единственная сторона, которая может это
/// узнать, — оболочка: на Windows ответ даёт WinRT (`NetworkInformation.GetConnectionCost`), на
/// Android это будет `ConnectivityManager`. Ядро факт только принимает.
///
/// Зачем: он решает выбор контроллера перегрузки. BBR везде, кроме метрируемых каналов —
/// правило и оба замера, на которых оно стоит, в `select_congestion_control`
/// (docs/bugs/cubic-caps-this-path-at-four-percent.md и uplink-goodput-retransmit.md §P1).
/// </summary>
internal static class LinkCostProbe
{
    internal const string Metered = "metered";
    internal const string Unmetered = "unmetered";
    internal const string Unknown = "unknown";

    // Числовые значения WinRT `NetworkCostType`. Держим их своими, чтобы правило можно было
    // проверить тестом без WinRT — и чтобы было видно, ЧТО именно мы считаем платным.
    internal const int CostUnknown = 0;
    internal const int CostUnrestricted = 1;
    internal const int CostFixed = 2;
    internal const int CostVariable = 3;

    /// <summary>
    /// Правило классификации. `Fixed`/`Variable` — тариф с лимитом или поштучной оплатой.
    /// Роуминг и превышенный лимит считаем платными ДАЖЕ на «безлимитном» типе: там байт стоит
    /// дороже всего, а именно цену байта мы и бережём.
    /// </summary>
    internal static string Classify(bool hasProfile, int costType, bool roaming, bool overDataLimit)
    {
        if (!hasProfile)
        {
            return Unknown;
        }

        if (roaming || overDataLimit)
        {
            return Metered;
        }

        return costType switch
        {
            CostFixed or CostVariable => Metered,
            CostUnrestricted => Unmetered,
            _ => Unknown,
        };
    }

    /// <summary>
    /// Спросить систему. Любая осечка — `unknown`, а не догадка: ядро само решит, что делать с
    /// незнанием, и это решение записано в одном месте.
    /// </summary>
    internal static string Read()
    {
        try
        {
            var profile = global::Windows.Networking.Connectivity.NetworkInformation
                .GetInternetConnectionProfile();
            if (profile is null)
            {
                return Unknown;
            }

            var cost = profile.GetConnectionCost();
            if (cost is null)
            {
                return Unknown;
            }

            return Classify(
                hasProfile: true,
                costType: (int)cost.NetworkCostType,
                roaming: cost.Roaming,
                overDataLimit: cost.OverDataLimit);
        }
        catch (Exception)
        {
            // WinRT недоступен (не тот контейнер, не та сборка) — это не повод падать и не повод
            // выдумывать ответ.
            return Unknown;
        }
    }
}
