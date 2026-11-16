using Omni.Windows.App.Core;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class TraceLogPreferenceTests
{
    [Fact]
    public void Load_TakesTheStoredSwitchAndFallsBackToTheBuildDefault()
    {
        string dir = Path.Combine(Path.GetTempPath(), $"omni-trace-pref-{Guid.NewGuid():N}");
        Directory.CreateDirectory(dir);
        try
        {
            string file = Path.Combine(dir, "device-preferences.json");
            Assert.Equal(TraceLogPreference.Default, TraceLogPreference.Load(dir));

            File.WriteAllText(file, "{\"theme\":\"dark\",\"traceLog\":true}");
            Assert.True(TraceLogPreference.Load(dir));

            File.WriteAllText(file, "{\"traceLog\":false}");
            Assert.False(TraceLogPreference.Load(dir));

            // A spoiled value or file is the default, never a crash at startup.
            File.WriteAllText(file, "{\"traceLog\":\"yes\"}");
            Assert.Equal(TraceLogPreference.Default, TraceLogPreference.Load(dir));
            File.WriteAllText(file, "{not json");
            Assert.Equal(TraceLogPreference.Default, TraceLogPreference.Load(dir));
        }
        finally
        {
            Directory.Delete(dir, recursive: true);
        }
    }
}
