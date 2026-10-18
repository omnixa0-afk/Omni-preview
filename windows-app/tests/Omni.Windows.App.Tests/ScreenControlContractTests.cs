using System.Text.Json;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Groups;
using Xunit;

namespace Omni.Windows.App.Tests;

/// The JSON the shell and the core exchange for remote screen control — the mirror of
/// `ffi_screen_control_json_contract_holds_both_ways` in omni-ffi. See
/// docs/decisions/remote-screen-control.md.
public sealed class ScreenControlContractTests
{
    [Fact]
    public void A_delivery_from_the_core_reads_into_the_injectors_shape()
    {
        // Exactly what omni-ffi's contract test pins as the core's output.
        const string json =
            """{"dialog_key":"group:g","viewer":"QUJD","pointer":[7,8],"events":[{"kind":"button","button":"left","down":true}],"release":false}""";
        ScreenControlDelivery? delivery = JsonSerializer.Deserialize(json, OmniJsonSerializerContext.Default.ScreenControlDelivery);

        Assert.NotNull(delivery);
        Assert.Equal(new ushort[] { 7, 8 }, delivery!.Pointer);
        ScreenInputEventDto click = Assert.Single(delivery.Events);
        Assert.Equal(("button", "left", true), (click.Kind, click.Button, click.Down));

        // Negative half: a release with no pointer and no events is still a valid delivery.
        ScreenControlDelivery? release = JsonSerializer.Deserialize(
            """{"dialog_key":"d","viewer":"v","pointer":null,"events":[],"release":true}""",
            OmniJsonSerializerContext.Default.ScreenControlDelivery);
        Assert.True(release!.Release);
        Assert.Null(release.Pointer);
        Assert.Empty(release.Events);
    }

    [Fact]
    public void The_shells_send_request_carries_only_each_events_own_fields()
    {
        string json = JsonSerializer.Serialize(
            new RustBridge.ScreenControlSendRequest(
                "dialog:x",
                null,
                new ushort[] { 1, 65535 },
                new[]
                {
                    ScreenInputEventDto.ButtonEvent("x2", true),
                    ScreenInputEventDto.WheelEvent(0, -120),
                    ScreenInputEventDto.KeyEvent(65, 30, true, false),
                }),
            OmniJsonSerializerContext.Default.ScreenControlSendRequest);

        // Compared as JSON, not as text: the serializer context indents.
        Assert.True(System.Text.Json.Nodes.JsonNode.DeepEquals(
            System.Text.Json.Nodes.JsonNode.Parse(
                """{"dialog_key":"dialog:x","pointer":[1,65535],"events":[{"kind":"button","button":"x2","down":true},{"kind":"wheel","dx":0,"dy":-120},{"kind":"key","down":false,"vk":65,"scan":30,"extended":true}]}"""),
            System.Text.Json.Nodes.JsonNode.Parse(json)), json);
        // Negative half: a 1:1 send names no sharer — the core reads a sharer only in a group.
        Assert.DoesNotContain("sharer", json);
    }

    [Fact]
    public void A_shared_screen_tile_is_letterboxed_and_offers_control_only_when_granted()
    {
        GroupCallParticipantItem tile = new() { MemberPub = "m" };
        Assert.Equal(Microsoft.UI.Xaml.Media.Stretch.UniformToFill, tile.VideoStretch);
        Assert.Equal(Microsoft.UI.Xaml.Visibility.Collapsed, tile.ControlButtonVisibility);

        tile.IsScreenSharing = true;
        tile.LetsMeControl = true;
        Assert.Equal(Microsoft.UI.Xaml.Media.Stretch.Uniform, tile.VideoStretch);
        Assert.Equal(Microsoft.UI.Xaml.Visibility.Visible, tile.ControlButtonVisibility);
        tile.IsControlling = true;
        Assert.Equal("Перестать управлять", tile.ControlButtonText);

        // Negative half: a withdrawn grant hides the button again.
        tile.LetsMeControl = false;
        Assert.Equal(Microsoft.UI.Xaml.Visibility.Collapsed, tile.ControlButtonVisibility);
    }

    [Fact]
    public void A_point_maps_through_the_fitted_pictures_own_rectangle()
    {
        // A 16:9 screen in a square surface: the picture is a band across the middle.
        (double X, double Y)? centre = MainWindow.FitPoint(800, 800, 1920, 1080, 400, 400);
        Assert.Equal((0.5, 0.5), centre);
        (double X, double Y)? corner = MainWindow.FitPoint(800, 800, 1920, 1080, 800, 400 + 225);
        Assert.Equal(1.0, corner!.Value.X, 3);
        Assert.Equal(1.0, corner.Value.Y, 3);

        // Negative half: a point on the bar is outside the picture (the viewer clamps it to the
        // edge), and with no picture size there is no mapping at all.
        Assert.True(MainWindow.FitPoint(800, 800, 1920, 1080, 400, 10)!.Value.Y < 0);
        Assert.Null(MainWindow.FitPoint(800, 800, 0, 1080, 400, 400));
    }

    [Fact]
    public void Remote_input_waits_a_moment_after_the_sharers_own_hand()
    {
        using var guard = new Omni.Windows.App.Features.Calls.LocalInputGuard();
        Assert.False(guard.ShouldYield(10_000));
        guard.NoteLocalInput(10_000);
        Assert.True(guard.ShouldYield(10_500));
        // Negative half: once the hand has been still for the yield window, remote input resumes.
        Assert.False(guard.ShouldYield(10_000 + Omni.Windows.App.Features.Calls.LocalInputGuard.YieldMs));
    }

    [Fact]
    public void Injected_input_is_not_mistaken_for_the_sharers_own_hand()
    {
        // The guard's hooks see every input; what SendInput puts in (ours, from any viewer) carries
        // the injected flag and must not count as local — or the sharer's machine would pause its
        // own remote control forever. One pixel right and back: the cursor ends where it was.
        using var guard = new Omni.Windows.App.Features.Calls.LocalInputGuard();
        guard.Arm();
        Assert.True(guard.IsArmed);
        Assert.Equal(0, guard.InstallError);
        Input there = new() { Type = 0, Mouse = new MouseInput { Dx = 1, Flags = 0x0001 } };
        Input back = new() { Type = 0, Mouse = new MouseInput { Dx = -1, Flags = 0x0001 } };
        Assert.Equal(2u, SendInput(2, new[] { there, back }, System.Runtime.InteropServices.Marshal.SizeOf<Input>()));
        Thread.Sleep(300);
        Assert.True(guard.EventsSeen > 0, "the hook saw the injected move");
        Assert.False(guard.ShouldYield(Environment.TickCount64));

        // Negative half: disarmed, the hooks are gone.
        guard.Disarm();
        Assert.False(guard.IsArmed);
    }

    [System.Runtime.InteropServices.StructLayout(System.Runtime.InteropServices.LayoutKind.Sequential)]
    private struct Input
    {
        public uint Type;
        public MouseInput Mouse;
    }

    [System.Runtime.InteropServices.StructLayout(System.Runtime.InteropServices.LayoutKind.Sequential)]
    private struct MouseInput
    {
        public int Dx;
        public int Dy;
        public uint MouseData;
        public uint Flags;
        public uint Time;
        public IntPtr ExtraInfo;
    }

    [System.Runtime.InteropServices.DllImport("user32.dll")]
    private static extern uint SendInput(uint count, Input[] inputs, int size);
}
