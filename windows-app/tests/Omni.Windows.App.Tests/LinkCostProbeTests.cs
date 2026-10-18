using Omni.Windows.App.Core;
using Xunit;

namespace Omni.Windows.App.Tests;

/// <summary>
/// Единственный признак, по которому расходятся два замера контроллера перегрузки: считают ли
/// трафик. Ошибка здесь стоит либо 18-кратной потери скорости (домашний канал принят за
/// метрируемый), либо трети трафика впустую (мобильный принят за безлимитный).
/// </summary>
public sealed class LinkCostProbeTests
{
    [Fact]
    public void Paid_Tariffs_Are_Metered()
    {
        Assert.Equal("metered", LinkCostProbe.Classify(true, LinkCostProbe.CostFixed, false, false));
        Assert.Equal("metered", LinkCostProbe.Classify(true, LinkCostProbe.CostVariable, false, false));
    }

    [Fact]
    public void Roaming_And_Over_Limit_Are_Metered_Even_On_An_Unrestricted_Plan()
    {
        // Там байт стоит дороже всего, а мы бережём именно цену байта.
        Assert.Equal("metered", LinkCostProbe.Classify(true, LinkCostProbe.CostUnrestricted, roaming: true, overDataLimit: false));
        Assert.Equal("metered", LinkCostProbe.Classify(true, LinkCostProbe.CostUnrestricted, roaming: false, overDataLimit: true));
    }

    [Fact]
    public void An_Unrestricted_Plan_Is_Unmetered()
    {
        Assert.Equal("unmetered", LinkCostProbe.Classify(true, LinkCostProbe.CostUnrestricted, false, false));
    }

    [Fact]
    public void Not_Knowing_Is_Reported_As_Unknown_Not_Guessed()
    {
        // Отрицательная половина: «нет профиля» и «система сама не знает» — это НЕ «безлимит».
        // Что делать с незнанием, решает ядро, и это решение записано в одном месте.
        Assert.Equal("unknown", LinkCostProbe.Classify(hasProfile: false, LinkCostProbe.CostUnrestricted, false, false));
        Assert.Equal("unknown", LinkCostProbe.Classify(true, LinkCostProbe.CostUnknown, false, false));
        Assert.Equal("unknown", LinkCostProbe.Classify(true, costType: 99, false, false));
    }

    [Fact]
    public void Reading_The_System_Never_Throws()
    {
        // Осечка WinRT не должна мешать сети подняться.
        string cost = LinkCostProbe.Read();
        Assert.Contains(cost, new[] { "metered", "unmetered", "unknown" });
    }
}
