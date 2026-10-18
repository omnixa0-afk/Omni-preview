namespace Omni.Windows.App.Features.Calls;

internal readonly record struct CallVideoHealthSample(
    string CaptureDiagnostics,
    long RemoteLossFraction,
    long DecodeStalls,
    long RenderStalls,
    long CaptureLoopFps = 0,
    int TargetFps = 0);

internal static class CallVideoHealthClassifier
{
    internal static string Classify(CallVideoHealthSample sample)
    {
        if (sample.CaptureDiagnostics.Contains("camera_contended", StringComparison.OrdinalIgnoreCase))
        {
            return "camera_contended";
        }

        if (sample.CaptureDiagnostics.Contains("camera_starting", StringComparison.OrdinalIgnoreCase))
        {
            return "camera_starting";
        }

        if (sample.CaptureDiagnostics.Contains("capture_unavailable", StringComparison.OrdinalIgnoreCase))
        {
            return "capture_unavailable";
        }

        if (sample.RemoteLossFraction >= 26)
        {
            return "network_loss";
        }

        if (sample.TargetFps >= 24 &&
            sample.CaptureLoopFps > 0 &&
            sample.CaptureLoopFps * 100 < sample.TargetFps * 70)
        {
            return "capture_throttled";
        }

        if (sample.DecodeStalls > 0)
        {
            return "decode_stall";
        }

        if (sample.RenderStalls > 0)
        {
            return "render_stall";
        }

        return "stable";
    }
}
