using Xunit;
using Omni.Windows.App.Presentation;
using Windows.UI;

namespace Omni.Windows.App.Tests;

public sealed class AvatarPaletteTests
{
    [Fact]
    public void TheSameIdAlwaysGetsTheSameHue_AndDifferentIdsSpreadOverTheWheel()
    {
        Assert.Equal(AvatarPalette.HueFor("acct-4d3ea570a8bf86d645e43995c2426847"), AvatarPalette.HueFor("acct-4d3ea570a8bf86d645e43995c2426847"));
        int distinct = Enumerable.Range(0, 200).Select(i => AvatarPalette.HueFor($"acct-{i:x8}")).Distinct().Count();
        Assert.True(distinct > 120, $"only {distinct} hues for 200 ids");
        Assert.DoesNotContain(Enumerable.Range(0, 500).Select(i => AvatarPalette.HueFor($"g-{i}")), h => h is >= 45 and < 85);
    }

    [Fact]
    public void EveryHueCarriesWhiteInitials_AndTheThemesGetDifferentShades()
    {
        for (int hue = 0; hue < 360; hue++)
        {
            foreach (bool dark in new[] { true, false })
            {
                Color c = AvatarPalette.ColorForHue(hue, dark);
                Assert.True(ContrastWithWhite(c) >= 3.5, $"hue {hue} dark={dark}: {ContrastWithWhite(c):0.00}");
            }

            Assert.NotEqual(AvatarPalette.ColorForHue(hue, true), AvatarPalette.ColorForHue(hue, false));
        }
    }

    private static double ContrastWithWhite(Color c) => 1.05 / (AvatarPalette.Luminance(c) + 0.05);
}
