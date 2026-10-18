using System.Text.Json;

namespace Omni.Windows.App.Core;

internal sealed class SidecarSettingsCache
{
    private readonly object syncRoot = new();
    private readonly Dictionary<string, CachedSidecarText> rawTextByPath = new(StringComparer.OrdinalIgnoreCase);

    public TEnum ReadEnum<TEnum>(
        string sidecarPath,
        TEnum defaultValue,
        string jsonPropertyName,
        Func<string, TEnum> parser)
    {
        CachedSidecarText cached = ReadRawText(sidecarPath);
        if (!cached.Exists || string.IsNullOrWhiteSpace(cached.RawText))
        {
            return defaultValue;
        }

        return parser(TryReadSidecarJsonStringProperty(cached.RawText, jsonPropertyName) ?? cached.RawText);
    }

    public void Invalidate(string sidecarPath)
    {
        lock (syncRoot)
        {
            rawTextByPath.Remove(sidecarPath);
        }
    }

    private CachedSidecarText ReadRawText(string sidecarPath)
    {
        FileInfo file = new(sidecarPath);
        if (!file.Exists)
        {
            lock (syncRoot)
            {
                if (rawTextByPath.TryGetValue(sidecarPath, out CachedSidecarText? cachedMissing) &&
                    cachedMissing is not null &&
                    !cachedMissing.Exists)
                {
                    return cachedMissing;
                }

                CachedSidecarText missing = new(false, DateTimeOffset.MinValue, 0, null);
                rawTextByPath[sidecarPath] = missing;
                return missing;
            }
        }

        DateTimeOffset lastWriteTimeUtc = file.LastWriteTimeUtc;
        long length = file.Length;

        lock (syncRoot)
        {
            if (rawTextByPath.TryGetValue(sidecarPath, out CachedSidecarText? cached) &&
                cached is not null &&
                cached.Exists &&
                cached.LastWriteTimeUtc == lastWriteTimeUtc &&
                cached.Length == length)
            {
                return cached;
            }
        }

        string rawText = File.ReadAllText(sidecarPath).Trim();
        CachedSidecarText refreshed = new(true, lastWriteTimeUtc, length, rawText);

        lock (syncRoot)
        {
            rawTextByPath[sidecarPath] = refreshed;
        }

        return refreshed;
    }

    private static string? TryReadSidecarJsonStringProperty(string raw, string propertyName)
    {
        if (!raw.StartsWith('{'))
        {
            return null;
        }

        try
        {
            using JsonDocument document = JsonDocument.Parse(raw);
            if (document.RootElement.ValueKind != JsonValueKind.Object)
            {
                return null;
            }

            if (document.RootElement.TryGetProperty(propertyName, out JsonElement propertyValue) &&
                propertyValue.ValueKind == JsonValueKind.String)
            {
                return propertyValue.GetString();
            }
        }
        catch (JsonException)
        {
            return null;
        }

        return null;
    }

    private sealed record CachedSidecarText(
        bool Exists,
        DateTimeOffset LastWriteTimeUtc,
        long Length,
        string? RawText);
}
