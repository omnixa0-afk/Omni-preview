using Omni.Windows.App.Core;
using Xunit;

namespace Omni.Windows.App.Tests;

/// <summary>
/// Настройка обязана либо сказать правду о том, что действует, либо молчать. Врущая строка здесь
/// хуже её отсутствия: весь смысл переключателя — A/B на одной живой сборке, а замер, который
/// показывает не тот контроллер, что едет, отравляет вывод.
/// </summary>
public sealed class CongestionControlPreferenceTests
{
    [Fact]
    public void Known_Modes_Survive_Case_And_Whitespace()
    {
        Assert.Equal("bbr", CongestionControlPreference.Normalize(" BBR "));
        Assert.Equal("cubic", CongestionControlPreference.Normalize("Cubic"));
        Assert.Equal("newreno", CongestionControlPreference.Normalize("NewReno"));
        Assert.Equal("auto", CongestionControlPreference.Normalize("auto"));
    }

    [Fact]
    public void Corrupt_Or_Missing_Value_Falls_Back_To_Auto()
    {
        // Отрицательная половина: испорченный файл настроек не должен запирать пользователя в
        // контроллере, которого он не выбирал.
        Assert.Equal("auto", CongestionControlPreference.Normalize(null));
        Assert.Equal("auto", CongestionControlPreference.Normalize(""));
        Assert.Equal("auto", CongestionControlPreference.Normalize("bbr2"));
    }

    [Fact]
    public void On_Auto_The_Status_Names_The_Controller_The_Rule_Picked()
    {
        // Именно здесь решение принимает не пользователь, поэтому строка обязана назвать итог.
        Assert.Equal(
            "Сейчас действует: BBR (выбрано по стоимости канала)",
            CongestionControlPreference.StatusText("auto", "bbr"));
        Assert.Equal(
            "Сейчас действует: CUBIC (выбрано по стоимости канала)",
            CongestionControlPreference.StatusText("auto", "cubic"));
    }

    [Fact]
    public void A_Manual_Choice_That_Actually_Applies_Is_Reported_Plainly()
    {
        Assert.Equal(
            "Сейчас действует: CUBIC",
            CongestionControlPreference.StatusText("cubic", "cubic"));
    }

    [Fact]
    public void A_Manual_Choice_Overridden_By_The_Env_Var_Must_Say_So()
    {
        // Переменная окружения сильнее переключателя нарочно. Молчать об этом нельзя: настройка
        // показывала бы одно, а соединение ехало на другом — ровно та ложь, ради которой строка
        // состояния и существует.
        string status = CongestionControlPreference.StatusText("bbr", "cubic");

        Assert.Contains("CUBIC", status);
        Assert.Contains("OMNI_QUIC_CC", status);
    }
}
