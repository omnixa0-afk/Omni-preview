using Omni.Windows.App.Core;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class BundleExtractionCleanupTests
{
    [Fact]
    public void OlderCopiesGoWhileTheCurrentOneAndOnesInUseStay()
    {
        string root = Path.Combine(Path.GetTempPath(), "omni-extract-cleanup-" + Guid.NewGuid().ToString("N"));
        string app = Path.Combine(root, ".net", "Omni.Windows.App");
        string current = Make(app, "current");
        string old = Make(app, "old");
        string inUse = Make(app, "in-use");
        string halfRemoved = Make(app, "x.1234abcd.removing");
        try
        {
            using (File.Open(Path.Combine(inUse, "omni_ffi.dll"), FileMode.Open, FileAccess.Read, FileShare.Read))
            {
                int removed = BundleExtractionCleanup.RemoveStaleCopies(current);

                Assert.Equal(2, removed);
                Assert.False(Directory.Exists(old));
                Assert.False(Directory.Exists(halfRemoved));
                Assert.True(File.Exists(Path.Combine(current, "omni_ffi.dll")), "the copy we run from stays");
                Assert.True(File.Exists(Path.Combine(inUse, "omni_ffi.dll")), "a copy another instance runs from stays whole");
            }
        }
        finally
        {
            Directory.Delete(root, recursive: true);
        }
    }

    private static string Make(string parent, string name)
    {
        string folder = Path.Combine(parent, name);
        Directory.CreateDirectory(folder);
        File.WriteAllText(Path.Combine(folder, "omni_ffi.dll"), "x");
        return folder;
    }
}
