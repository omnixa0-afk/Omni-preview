using Omni.Windows.App;
using Omni.Windows.App.Core;
using Omni.Windows.App.ViewModels;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class BackgroundRefreshPolicyTests
{
    [Fact]
    public void SectionVisibility_FollowsWakeOnlyPlan()
    {
        AssertSectionVisibility(AuthenticatedShellSection.Chats, false, true, false, false);
        AssertSectionVisibility(AuthenticatedShellSection.Contacts, true, false, true, false);
        AssertSectionVisibility(AuthenticatedShellSection.Settings, true, false, false, true);
        AssertSectionVisibility(AuthenticatedShellSection.Diagnostics, true, true, false, true);
    }

    [Fact]
    public void ForegroundRefreshHelpers_PreferCompositeSnapshotsWhenCached()
    {
        string refreshSource = File.ReadAllText(ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.Refresh.cs"));
        string transportSource = File.ReadAllText(ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.Transport.cs"));

        Assert.Contains("lastQueueDiagnosticsSnapshot ?? rustBridge.GetQueueDiagnostics()", refreshSource, StringComparison.Ordinal);
        Assert.Contains("lastQueueDiagnosticsSnapshot = refresh.QueueDiagnosticsSnapshot;", transportSource, StringComparison.Ordinal);
        Assert.DoesNotContain("DirectRouteDiagnosticsSummary", transportSource, StringComparison.Ordinal);
    }

    [Fact]
    public void BackgroundRefreshScheduling_UsesVisibilityAwareWakeOnlyDomains()
    {
        string source = File.ReadAllText(ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.BackgroundRefresh.cs"));

        Assert.Contains("SectionShowsTransportDiscoverySurface(section)", source, StringComparison.Ordinal);
        Assert.Contains("IsWakeOrVisibleCadenceDue(", source, StringComparison.Ordinal);
        Assert.DoesNotContain(
            "nextLifecycleRefreshAt,\r\n            nextTransportRefreshAt,\r\n            nextDiscoveryRefreshAt",
            source,
            StringComparison.Ordinal);
    }

    private static void AssertSectionVisibility(
        AuthenticatedShellSection section,
        bool showsTransportDiscovery,
        bool showsQueue,
        bool showsContacts,
        bool showsFootprint)
    {
        Assert.Equal(showsTransportDiscovery, MainWindow.SectionShowsTransportDiscoverySurface(section));
        Assert.Equal(showsQueue, MainWindow.SectionShowsQueueSurface(section));
        Assert.Equal(showsContacts, MainWindow.SectionShowsContactsSurface(section));
        Assert.Equal(section == AuthenticatedShellSection.Settings, MainWindow.SectionShowsSettingsSurface(section));
        Assert.Equal(showsFootprint, MainWindow.SectionShowsFootprintSurface(section));
    }

    private static string ResolveRepoPath(params string[] relativeSegments)
    {
        DirectoryInfo? current = new(AppContext.BaseDirectory);
        while (current is not null &&
               !string.Equals(current.Name, "windows-app", StringComparison.OrdinalIgnoreCase))
        {
            current = current.Parent;
        }

        Assert.NotNull(current);

        string[] segments =
        [
            current!.FullName,
            .. relativeSegments,
        ];

        return Path.GetFullPath(Path.Combine(segments));
    }
}
