using System.Text.Json;
using Omni.Windows.App.Core;
using Omni.Windows.App.Presentation;
using Omni.Windows.App.ViewModels;

namespace Omni.Windows.App;

public sealed partial class MainWindow
{
    // The default encoder escapes every non-ASCII character, so a device named "Laptop (копия 2)"
    // read "Laptop (ко…)" in the payload a person reads and compares. Same JSON, the
    // text left as text; it only ever lands in a read-only text box, never in markup.
    private static readonly OmniJsonSerializerContext ReadableJson = new(new JsonSerializerOptions
    {
        PropertyNameCaseInsensitive = true,
        DefaultIgnoreCondition = System.Text.Json.Serialization.JsonIgnoreCondition.WhenWritingNull,
        WriteIndented = true,
        Encoder = System.Text.Encodings.Web.JavaScriptEncoder.UnsafeRelaxedJsonEscaping,
    });

    private void UpdateIdentityHeader()
    {
        IdentitySubtitleTextBlock.Text = MainWindowPresentation.BuildIdentitySubtitle(
            windowState.Onboarding.LocalAccount,
            windowState.Onboarding.LocalDevice);
        RenderIdentityAvatar();
        SettingsAccountSummaryTextBlock.Text = MainWindowPresentation.BuildAccountSummary(windowState.Onboarding.LocalAccount);
        SettingsProfileSummaryTextBlock.Text = BuildCurrentProfileSummaryText();
        RenderLocalPublicProfileEditor();
        RenderRecoverySummary();

        try
        {
            if (windowState.Onboarding.ShellMode == AppShellMode.Authenticated)
            {
                RefreshPasswordProtectionSummary();
                MyVerificationPayloadV1 payload = rustBridge.GetMyVerificationPayload();
                MyVerificationPayloadTextBox.Text = JsonSerializer.Serialize(
                    payload,
                    ReadableJson.MyVerificationPayloadV1);
                RenderRemoteAccessSettings();
                RenderOwnDevicesSettings();
            }
        }
        catch
        {
            // Leave last rendered settings state if verification payload is temporarily unavailable.
        }
    }

    private void RenderRecoverySummary()
    {
        SettingsRecoverySummaryTextBlock.Text = MainWindowPresentation.BuildRecoverySummary(
            windowState.Onboarding.LocalAccount,
            currentPasswordProtectionSummary,
            currentPasswordProtectionErrorText);
    }
}
