using System.Runtime.InteropServices;
using System.Text.Json;

namespace Omni.Windows.App.Core;

/// <summary>
/// Shared helper for reading + freeing native error strings returned from
/// FFI exports. Handles both legacy plain-text errors and the new JSON
/// envelope (see docs/decisions/ffi-error-envelope-unification.md).
///
/// Wave 2b (2026-05-16): extracted from CallVideoService so RustBridge
/// (binary control plane) can share the same parser.
/// </summary>
internal static class NativeErrorReader
{
    internal static string ReadAndFree(IntPtr error)
    {
        try
        {
            string raw = Marshal.PtrToStringUTF8(error) ?? "Native media operation failed.";
            if (raw.StartsWith("{", StringComparison.Ordinal))
            {
                try
                {
                    using JsonDocument doc = JsonDocument.Parse(raw);
                    if (doc.RootElement.TryGetProperty("error", out JsonElement errElem) &&
                        errElem.ValueKind == JsonValueKind.Object &&
                        errElem.TryGetProperty("message", out JsonElement msgElem) &&
                        msgElem.ValueKind == JsonValueKind.String)
                    {
                        return msgElem.GetString() ?? raw;
                    }
                }
                catch (JsonException)
                {
                    // Not valid JSON — fall through to raw.
                }
            }
            return raw;
        }
        finally
        {
            NativeMethods.omni_string_free(error);
        }
    }
}
