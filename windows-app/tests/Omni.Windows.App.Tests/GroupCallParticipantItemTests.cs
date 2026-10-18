using Microsoft.UI.Xaml;
using Omni.Windows.App.Features.Groups;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class GroupCallParticipantItemTests
{
    // A screen sharer sends video with the camera off — the tile must keep showing it
    // (it used to hide on "camera off" alone, leaving a sharer as an empty avatar).
    [Fact]
    public void ScreenSharingKeepsTheVideoVisibleWithTheCameraOff()
    {
        GroupCallParticipantItem tile = new() { IsCameraOff = true, IsScreenSharing = true };
        Assert.Equal(Visibility.Visible, tile.VideoVisibility);
        Assert.Equal(Visibility.Visible, tile.CameraOffBadgeVisibility);
    }

    // Negative half: nothing sent (camera off, no share) — the avatar shows, not a frozen frame.
    [Fact]
    public void NothingSentHidesTheVideo()
    {
        GroupCallParticipantItem tile = new() { IsCameraOff = true };
        Assert.Equal(Visibility.Collapsed, tile.VideoVisibility);
        tile.IsScreenSharing = true;
        Assert.Equal(Visibility.Visible, tile.VideoVisibility);
    }
}
