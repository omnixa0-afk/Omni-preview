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
        RefreshAccountSettingsAsync();
    }

    private sealed record AccountSettingsRead(
        string? ProfileId,
        PasswordProtectionRefreshPayload Password,
        string? Verification,
        List<ContactRemoteAccessRecord>? Granted,
        Dictionary<string, string>? Names,
        List<OwnDeviceView>? Devices);

    private bool accountSettingsReading;
    private bool accountSettingsReadAgain;
    private string shownRemoteAccess = string.Empty;
    private string shownOwnDevices = string.Empty;

    // What Settings and Diagnostics show about the account. The header above is updated on every section
    // switch and refresh tick: read here on the UI thread, this held the window 50-120 ms each time, the
    // whole contact list included, in sections that show none of it. So: off the UI thread, only where it
    // is seen (once anywhere: the password state also decides what other screens offer), and the lists
    // are rebuilt only when they changed.
    private async void RefreshAccountSettingsAsync()
    {
        if (windowState.Onboarding.ShellMode != AppShellMode.Authenticated ||
            !(ShouldRefreshCurrentProfileFootprintImmediately() || currentPasswordProtectionSummary is null))
        {
            return;
        }

        if (accountSettingsReading)
        {
            accountSettingsReadAgain = true;
            return;
        }

        accountSettingsReading = true;
        try
        {
            do
            {
                accountSettingsReadAgain = false;
                string? profileId = GetCurrentProfileId();
                AccountSettingsRead read = await Task.Run(() => ReadAccountSettings(profileId));
                if (string.Equals(read.ProfileId, GetCurrentProfileId(), StringComparison.Ordinal) &&
                    windowState.Onboarding.ShellMode == AppShellMode.Authenticated)
                {
                    ShowAccountSettings(read);
                }
            }
            while (accountSettingsReadAgain);
        }
        catch (Exception ex)
        {
            // Leave the last rendered state; the next tick reads again.
            AppendNavRefreshLog("async.account_settings", ex);
        }
        finally
        {
            accountSettingsReading = false;
        }
    }

    private AccountSettingsRead ReadAccountSettings(string? profileId)
    {
        string? verification = null;
        try
        {
            verification = JsonSerializer.Serialize(rustBridge.GetMyVerificationPayload(), ReadableJson.MyVerificationPayloadV1);
        }
        catch
        {
            // Temporarily unavailable: the box keeps what it shows.
        }

        (List<ContactRemoteAccessRecord>? granted, Dictionary<string, string>? names) = ReadRemoteAccess();
        return new AccountSettingsRead(
            profileId,
            FetchPasswordProtectionRefreshPayload(profileId),
            verification,
            granted,
            names,
            ReadOwnDevices());
    }

    private void ShowAccountSettings(AccountSettingsRead read)
    {
        ApplyPasswordProtectionRefreshPayload(read.Password);
        if (read.Verification is not null && MyVerificationPayloadTextBox.Text != read.Verification)
        {
            MyVerificationPayloadTextBox.Text = read.Verification;
        }

        if (read.Granted is not null && read.Names is not null)
        {
            string remoteAccess = string.Join('\n', read.Granted) + '\n' + string.Join('\n', read.Names);
            if (remoteAccess != shownRemoteAccess)
            {
                shownRemoteAccess = remoteAccess;
                ShowRemoteAccess(read.Granted, read.Names);
            }
        }

        if (read.Devices is not null)
        {
            string devices = string.Join('\n', read.Devices);
            if (devices != shownOwnDevices)
            {
                shownOwnDevices = devices;
                ShowOwnDevices(read.Devices);
            }
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
