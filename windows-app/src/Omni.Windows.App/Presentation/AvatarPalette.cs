using Microsoft.UI.Xaml.Media;
using Windows.UI;

namespace Omni.Windows.App.Presentation;

/// <summary>
/// The colour of an avatar without a photo, from the person's account id (a group's from its id):
/// the hue comes from the id — any of 360 — and saturation / lightness are fixed per theme, so
/// every avatar is equally calm and white initials read on all of them (owner, 2026-10-11:
/// «динамическая палитра через HSL», «на светлой и тёмной теме — приятно, не вырвиглазно»). The
/// same person has the same colour in the chat list, the chat, the contacts and a call, on every
/// device. The hash is FNV-1a over the UTF-8 bytes: stable across runs and machines
/// (string.GetHashCode is randomised per process).
/// </summary>
internal static class AvatarPalette
{
    // Saturation per theme, and the relative luminance every hue is brought to: equal luminance
    // makes all hues look equally bright (HSL lightness alone does not: yellow at the lightness of
    // blue glares), and keeps white initials readable on every one (contrast ≥ 3.8 light, ≥ 6 dark).
    private const double DarkSaturation = 0.40;
    private const double DarkLuminance = 0.11;
    private const double LightSaturation = 0.55;
    private const double LightLuminance = 0.22;

    // Hues 45–85 (mustard, olive) turn muddy at a luminance that carries white text: skipped.
    private const int MuddyHueStart = 45;
    private const int MuddyHueWidth = 40;

    private static bool dark = true;

    // One shared brush per hue: a theme switch recolours them in place, so every avatar on screen
    // follows without being rebuilt.
    private static readonly Dictionary<int, SolidColorBrush> Brushes = new();

    internal static int HueFor(string? seed)
    {
        if (string.IsNullOrEmpty(seed))
        {
            return 215; // the app's slate blue
        }

        uint hash = 2166136261;
        foreach (byte b in System.Text.Encoding.UTF8.GetBytes(seed))
        {
            hash = (hash ^ b) * 16777619;
        }

        int hue = (int)(hash % (360 - MuddyHueWidth));
        return hue >= MuddyHueStart ? hue + MuddyHueWidth : hue;
    }

    internal static Color ColorFor(string? seed) => ColorForHue(HueFor(seed), dark);

    internal static Color ColorForHue(int hue, bool darkTheme)
    {
        double saturation = darkTheme ? DarkSaturation : LightSaturation;
        double target = darkTheme ? DarkLuminance : LightLuminance;
        // Lightness that gives the target luminance at this hue (luminance grows with lightness).
        double low = 0, high = 1;
        for (int i = 0; i < 24; i++)
        {
            double mid = (low + high) / 2;
            if (Luminance(FromHsl(hue, saturation, mid)) < target)
            {
                low = mid;
            }
            else
            {
                high = mid;
            }
        }

        return FromHsl(hue, saturation, low);
    }

    internal static double Luminance(Color c)
    {
        static double Channel(byte v)
        {
            double s = v / 255.0;
            return s <= 0.03928 ? s / 12.92 : Math.Pow((s + 0.055) / 1.055, 2.4);
        }

        return 0.2126 * Channel(c.R) + 0.7152 * Channel(c.G) + 0.0722 * Channel(c.B);
    }

    /// UI thread only — a brush is a WinRT object.
    internal static SolidColorBrush BrushFor(string? seed)
    {
        int hue = HueFor(seed);
        if (!Brushes.TryGetValue(hue, out SolidColorBrush? brush))
        {
            brush = new SolidColorBrush(ColorForHue(hue, dark));
            Brushes[hue] = brush;
        }

        return brush;
    }

    /// The window's theme changed: the shared brushes take the other theme's shade in place.
    internal static void ApplyTheme(bool darkTheme)
    {
        dark = darkTheme;
        foreach ((int hue, SolidColorBrush brush) in Brushes)
        {
            brush.Color = ColorForHue(hue, dark);
        }
    }

    private static Color FromHsl(double hue, double saturation, double lightness)
    {
        double chroma = (1 - Math.Abs(2 * lightness - 1)) * saturation;
        double h = hue / 60.0;
        double x = chroma * (1 - Math.Abs(h % 2 - 1));
        (double r, double g, double b) = h switch
        {
            < 1 => (chroma, x, 0.0),
            < 2 => (x, chroma, 0.0),
            < 3 => (0.0, chroma, x),
            < 4 => (0.0, x, chroma),
            < 5 => (x, 0.0, chroma),
            _ => (chroma, 0.0, x),
        };
        double m = lightness - chroma / 2;
        return Color.FromArgb(
            0xFF,
            (byte)Math.Round((r + m) * 255),
            (byte)Math.Round((g + m) * 255),
            (byte)Math.Round((b + m) * 255));
    }
}
