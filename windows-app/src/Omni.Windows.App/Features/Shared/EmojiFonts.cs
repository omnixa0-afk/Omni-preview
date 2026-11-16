using System.IO;
using System.IO.Compression;
using System.Text.Json.Nodes;
using Microsoft.UI.Xaml.Media;

namespace Omni.Windows.App.Features.Shared;

/// The emoji style chosen in Settings → Общие, per device (device-preferences.json, key "emojiStyle").
/// A style is a colour font inside the assembly, unpacked on first use. WinUI reads a font file only by
/// ms-appx, which here means beside the exe: absolute paths and the data folder are silently ignored
/// (tried live 2026-11-04), so the folder goes there. Where that cannot be written, emoji stay the system's.
/// Text keeps its own font: the family lists it first, and only what it lacks comes from here.
public static class EmojiFonts
{
    internal const string PreferenceKey = "emojiStyle";

    // The text fonts of the app (WinUI's default, and Windows 10's, which has no Variable): neither has
    // emoji. Without the second, Windows 10 took digits and spaces from the emoji font and drew them blank.
    private const string TextFont = "Segoe UI Variable, Segoe UI";

    // Noto is COLR v1, which Windows 10 draws empty (seen live on 19045), so there the style is not offered
    // and Twemoji, COLR v0, stands in. The bitmap build of Noto did draw there, at 10 MB more in every exe:
    // the owner chose not to carry it (2026-11-04).
    private const int FirstBuildWithColrV1 = 22621;

    internal static bool NotoDrawnHere => Environment.OSVersion.Version.Build >= FirstBuildWithColrV1;

    internal static string DefaultStyle => NotoDrawnHere ? "noto" : "twemoji";

    private static string folder = string.Empty;
    private static string current = "system";

    /// The family for text that may hold emoji.
    public static FontFamily Family { get; private set; } = new(TextFont);

    /// The chosen style changed: what shows emoji picks up `Family`.
    internal static event Action? Changed;

    internal static string Current => current;

    /// Before the window: the themed resource makes every button and list row take the family too.
    internal static void Initialize(string appDirectory)
    {
        folder = Path.Combine(Path.GetDirectoryName(Environment.ProcessPath) ?? AppContext.BaseDirectory, "emoji-fonts");
        string style = DefaultStyle;
        try
        {
            string path = Path.Combine(appDirectory, "device-preferences.json");
            if (File.Exists(path)
                && JsonNode.Parse(File.ReadAllText(path)) is JsonObject document
                && document[PreferenceKey] is JsonValue value
                && value.TryGetValue(out string? stored)
                && stored is { Length: > 0 })
            {
                style = stored;
            }
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or System.Text.Json.JsonException)
        {
            // An unreadable file: the default, as for every other device preference.
        }

        Apply(style);
        Microsoft.UI.Xaml.Application.Current.Resources["ContentControlThemeFontFamily"] = Family;
    }

    internal static void Set(string style)
    {
        Apply(style);
        Microsoft.UI.Xaml.Application.Current.Resources["ContentControlThemeFontFamily"] = Family;
        Changed?.Invoke();
    }

    /// A style's family without choosing it: the preview in Settings.
    internal static FontFamily FamilyOf(string style) => style switch
    {
        "noto" => Composite("noto", "Noto Color Emoji"),
        "twemoji" => Composite("twemoji", "Twemoji Mozilla"),
        _ => new FontFamily(TextFont),
    };

    private static void Apply(string style)
    {
        current = style == "noto" && !NotoDrawnHere ? "twemoji" : style is "noto" or "twemoji" ? style : "system";
        Family = FamilyOf(current);
    }

    private static FontFamily Composite(string file, string name)
    {
        string? path = Unpacked(file);
        return path is null ? new FontFamily(TextFont) : new FontFamily($"{TextFont}, ms-appx:///emoji-fonts/{file}.ttf#{name}");
    }

    // Written whole under a temporary name, so a crash leaves no half a font.
    private static string? Unpacked(string file)
    {
        string path = Path.Combine(folder, file + ".ttf");
        if (File.Exists(path))
        {
            return path;
        }

        try
        {
            Directory.CreateDirectory(folder);
            using Stream? packed = typeof(EmojiFonts).Assembly.GetManifestResourceStream($"Omni.Windows.App.Emoji.{file}.ttf.gz");
            if (packed is null)
            {
                return null;
            }

            string temporary = path + ".tmp";
            using (GZipStream unpacked = new(packed, CompressionMode.Decompress))
            using (FileStream target = File.Create(temporary))
            {
                unpacked.CopyTo(target);
            }

            File.Move(temporary, path, overwrite: true);
            return path;
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or InvalidDataException)
        {
            // No such style today: emoji stay the system's.
            return null;
        }
    }
}
