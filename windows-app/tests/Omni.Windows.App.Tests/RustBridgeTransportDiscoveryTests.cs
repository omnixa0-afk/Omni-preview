using Omni.Windows.App.Core;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class RustBridgeTransportDiscoveryTests
{
    [Fact]
    public void CollectSnapshotsSkippingFailures_KeepsHealthyEntries_WhenOneProjectionFails()
    {
        string[] adapters = ["Wi-Fi", "Broken VPN", "Ethernet"];

        List<string> snapshots = RustBridge.CollectSnapshotsSkippingFailures(
            adapters,
            adapter => adapter == "Broken VPN"
                ? throw new InvalidOperationException("boom")
                : adapter.ToUpperInvariant(),
            adapter => adapter);

        Assert.Equal(["WI-FI", "ETHERNET"], snapshots);
    }
}
