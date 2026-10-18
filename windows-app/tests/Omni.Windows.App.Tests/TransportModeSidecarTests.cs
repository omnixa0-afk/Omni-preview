using System.Text.Json;
using Omni.Windows.App.Core;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class TransportModeSidecarTests
{
    [Fact]
    public void TransportModeSidecar_DefaultsToQuic_WhenMissing()
    {
        string appDirectory = CreateTempAppDirectory();
        try
        {
            TransportMode mode = RustBridge.ReadTransportModeFromSidecar(appDirectory);

            Assert.Equal(TransportMode.Quic, mode);
        }
        finally
        {
            Directory.Delete(appDirectory, recursive: true);
        }
    }

    private static string CreateTempAppDirectory()
    {
        string directory = Path.Combine(Path.GetTempPath(), $"omni-sidecar-test-{Guid.NewGuid():N}");
        Directory.CreateDirectory(directory);
        return directory;
    }
}
