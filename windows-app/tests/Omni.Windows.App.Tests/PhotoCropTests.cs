using Omni.Windows.App.Features.Shared;
using Xunit;

namespace Omni.Windows.App.Tests;

/// The crop frame's output: what the frame shows of the turned picture, read back from the source.
public sealed class PhotoCropTests
{
    [Fact]
    public void A_quarter_turn_puts_the_left_of_the_picture_on_top()
    {
        // 4x2, left half red, right half blue (BGRA). Turned clockwise it is 2 wide, 4 tall, red on top;
        // at the covering zoom the frame shows a 2x2 square of it.
        byte[] pixels = new byte[4 * 2 * 4];
        for (int y = 0; y < 2; y++)
        {
            for (int x = 0; x < 4; x++)
            {
                int at = (y * 4 + x) * 4;
                pixels[at + (x < 2 ? 2 : 0)] = 255;
                pixels[at + 3] = 255;
            }
        }

        double cover = 320.0 / 2;
        byte[] top = PhotoCropDialog.Sample(pixels, 4, 2, turns: 1, cover, centreX: 1, centreY: 1, side: 2);
        Assert.All(Enumerable.Range(0, 4), i => Assert.Equal((0, 0, 255), (top[i * 4], top[i * 4 + 1], top[i * 4 + 2])));

        // Negative half: the bottom of the turned picture is the source's right — blue — and unturned
        // the same frame shows the red left.
        byte[] bottom = PhotoCropDialog.Sample(pixels, 4, 2, turns: 1, cover, centreX: 1, centreY: 3, side: 2);
        Assert.All(Enumerable.Range(0, 4), i => Assert.Equal((255, 0, 0), (bottom[i * 4], bottom[i * 4 + 1], bottom[i * 4 + 2])));
        byte[] unturned = PhotoCropDialog.Sample(pixels, 4, 2, turns: 0, cover, centreX: 1, centreY: 1, side: 2);
        Assert.Equal(255, unturned[2]);
    }
}
