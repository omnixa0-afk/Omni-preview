using System.Text.Json;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Shared;
using Omni.Windows.App.Presentation;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class VoiceMessageTests
{
    [Fact]
    public void TheOutlineHasOneBarPerStretchAndTheLoudestIsFull()
    {
        float[] levels = Enumerable.Range(0, 640).Select(i => i < 320 ? 0.01f : 0.04f).ToArray();
        byte[] bars = MainWindowPresentation.BuildVoiceWaveform(levels, 64);
        Assert.Equal(64, bars.Length);
        Assert.Equal(255, bars.Max());
        Assert.True(bars[0] < bars[63], "the quiet half draws lower");
    }

    [Fact]
    public void AShortRecordingHasAsManyBarsAsLevelsAndSilenceDrawsFlat()
    {
        Assert.Equal(3, MainWindowPresentation.BuildVoiceWaveform([0.1f, 0.2f, 0.3f], 64).Length);
        Assert.All(MainWindowPresentation.BuildVoiceWaveform([0f, 0f], 64), bar => Assert.Equal(0, bar));
        Assert.All(MainWindowPresentation.BuildVoiceWaveform([0.0002f, 0.0002f], 64), bar => Assert.True(bar < 40, "room hum draws low"));
        Assert.Empty(MainWindowPresentation.BuildVoiceWaveform([], 64));
    }

    [Theory]
    [InlineData(0, "0:00")]
    [InlineData(999, "0:00")]
    [InlineData(67_000, "1:07")]
    [InlineData(900_000, "15:00")]
    [InlineData(-5, "0:00")]
    public void TheTimeReadsAsMinutesAndSeconds(int milliseconds, string expected)
        => Assert.Equal(expected, MainWindowPresentation.FormatVoiceTime(milliseconds));

    [Fact]
    public void ABrokenOutlineFromTheCoreDrawsEvenly()
    {
        Assert.Empty(MainWindowPresentation.DecodeVoiceWaveform("not base64!"));
        Assert.Empty(MainWindowPresentation.DecodeVoiceWaveform(null));
        Assert.Equal(new byte[] { 1, 2 }, MainWindowPresentation.DecodeVoiceWaveform(Convert.ToBase64String([1, 2])));
    }

    [Fact]
    public void TheWaveIsAWholeHeaderAndThePcm()
    {
        byte[] wave = VoicePlayer.BuildWave([1, -1, 300], 3, 24_000);
        Assert.Equal(44 + 6, wave.Length);
        Assert.Equal("RIFF", System.Text.Encoding.ASCII.GetString(wave, 0, 4));
        Assert.Equal("WAVEfmt ", System.Text.Encoding.ASCII.GetString(wave, 8, 8));
        Assert.Equal(24_000, BitConverter.ToInt32(wave, 24));
        Assert.Equal(6, BitConverter.ToInt32(wave, 40));
        Assert.Equal(300, BitConverter.ToInt16(wave, 48));
    }

    [Fact]
    public void TheCoresVoicePresentationReadsAsAVoiceMessage()
    {
        AttachmentPresentation? shown = JsonSerializer.Deserialize<AttachmentPresentation>(
            """{"kind":"voice","width":0,"height":0,"duration_ms":4200,"waveform_b64":"AQI="}""");
        Assert.NotNull(shown);
        Assert.True(shown!.IsVoice);
        Assert.False(shown.IsPhoto);
        Assert.Equal(4200, shown.DurationMs);
    }

    [Fact]
    public void TheCoresVideoMessageReadsAsOne()
    {
        AttachmentPresentation? shown = JsonSerializer.Deserialize<AttachmentPresentation>(
            """{"kind":"video_note","width":854,"height":480,"duration_ms":4200,"preview_jpeg_b64":"AQI="}""");
        Assert.NotNull(shown);
        Assert.True(shown!.IsVideoNote);
        Assert.False(shown.IsVoice);
        Assert.False(shown.IsPhoto);
        Assert.Equal(4200, shown.DurationMs);
    }
}
