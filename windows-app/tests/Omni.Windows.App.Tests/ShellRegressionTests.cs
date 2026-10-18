using System.Text.RegularExpressions;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class ShellRegressionTests
{
    [Fact]
    public void MainWindowXamlCodeBehind_OnlyKeepsShellWindowHandlers()
    {
        string filePath = ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.xaml.cs");
        string source = File.ReadAllText(filePath);

        string[] handlers = Regex.Matches(source, @"private\s+(?:async\s+)?void\s+(On[A-Za-z0-9_]+)\s*\(")
            .Select(match => match.Groups[1].Value)
            .OrderBy(name => name, StringComparer.Ordinal)
            .ToArray();

        // Window lifecycle only. OnAppWindowClosing belongs here with the other two: the teardown
        // has to run while the window's dispatcher is still alive, which is a property of the
        // window, not of any feature.
        Assert.Equal(
            new[] { "OnActivated", "OnAppWindowClosing", "OnClosed" },
            handlers);
    }

    [Fact]
    public void ChatRuntimeSurface_DoesNotContainLegacyMethods()
    {
        string filePath = ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.Chats.cs");
        string source = File.ReadAllText(filePath);

        Assert.DoesNotContain("RefreshMessagesLegacy", source, StringComparison.Ordinal);
        Assert.DoesNotContain("RefreshChatsListLegacy", source, StringComparison.Ordinal);
        Assert.DoesNotContain("BuildChatThreadItemsLegacy", source, StringComparison.Ordinal);
        Assert.DoesNotContain("RefreshChatHeaderSummaryLegacy", source, StringComparison.Ordinal);
    }

    [Fact]
    public void ChatRefreshSurface_UsesInPlaceMessageAndSidebarPatching()
    {
        string chatsSource = File.ReadAllText(ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.Chats.cs"));
        string handlersSource = File.ReadAllText(ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.ChatHandlers.cs"));
        string composerSource = File.ReadAllText(ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.ChatComposer.cs"));

        Assert.DoesNotContain("SynchronizeCollection(messageItems", chatsSource, StringComparison.Ordinal);
        Assert.DoesNotContain("MainWindowPresentation.ToChatItem(", chatsSource, StringComparison.Ordinal);
        Assert.Contains("currentChatMessageItemsById", chatsSource, StringComparison.Ordinal);
        Assert.Contains("RebuildTranscriptFromPayload(payload);", chatsSource, StringComparison.Ordinal);
        Assert.Contains("ApplyIncrementalTranscriptDelta(payload);", chatsSource, StringComparison.Ordinal);
        Assert.Contains("PatchChatThreadCollection(threads);", chatsSource, StringComparison.Ordinal);
        Assert.Contains("currentSelected?.DialogKey", chatsSource, StringComparison.Ordinal);
        Assert.DoesNotContain("RefreshChatsList(true, GetResolvedCurrentDialogKey())", handlersSource, StringComparison.Ordinal);
        Assert.DoesNotContain("RefreshChatsList(true, GetResolvedCurrentDialogKey())", composerSource, StringComparison.Ordinal);
    }

    [Fact]
    public void ChatForgetContactAction_UsesHardResetBridge()
    {
        string handlersSource = File.ReadAllText(ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.ChatHandlers.cs"));
        string bridgeSource = File.ReadAllText(ResolveRepoPath("src", "Omni.Windows.App", "Core", "RustBridge.AccountContacts.cs"));

        Assert.Contains("ForgetContactAndResetPeer(", handlersSource, StringComparison.Ordinal);
        Assert.DoesNotContain("rustBridge.RemoveContact(currentChatHeaderSummary.ContactCard.Contact.ContactId)", handlersSource, StringComparison.Ordinal);
        Assert.Contains("contacts_forget_and_reset_peer", bridgeSource, StringComparison.Ordinal);
    }

    [Fact]
    public void MainWindowXamlCodeBehind_IsReducedToShellSurface()
    {
        string filePath = ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.xaml.cs");
        string source = File.ReadAllText(filePath);
        string[] methods = Regex.Matches(
                source,
                @"private\s+(?:async\s+)?(?:static\s+)?(?:[\w<>,\[\]\.?]+)\s+([A-Za-z0-9_]+)\s*\(")
            .Select(match => match.Groups[1].Value)
            .OrderBy(name => name, StringComparer.Ordinal)
            .ToArray();
        int lineCount = File.ReadLines(filePath).Count();

        Assert.Equal(
            new[] { "BootAsync", "OnActivated", "OnAppWindowClosing", "OnClosed", "ShutDownThenCloseAsync" },
            methods);
        Assert.InRange(lineCount, 1, 700);
    }

    [Fact]
    public void MainWindowXaml_IsShellCompositionOnly()
    {
        string filePath = ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.xaml");
        string source = File.ReadAllText(filePath);

        Assert.Contains("OnboardingShellControl", source, StringComparison.Ordinal);
        Assert.Contains("ChatsSectionControl", source, StringComparison.Ordinal);
        Assert.Contains("ContactsSectionControl", source, StringComparison.Ordinal);
        Assert.Contains("SettingsSectionControl", source, StringComparison.Ordinal);
        Assert.Contains("DiagnosticsSectionControl", source, StringComparison.Ordinal);

        Assert.DoesNotContain("x:Name=\"ChatsSectionRoot\"", source, StringComparison.Ordinal);
        Assert.DoesNotContain("x:Name=\"ContactsSectionRoot\"", source, StringComparison.Ordinal);
        Assert.DoesNotContain("x:Name=\"DevicesSectionRoot\"", source, StringComparison.Ordinal);
        Assert.DoesNotContain("x:Name=\"SettingsSectionRoot\"", source, StringComparison.Ordinal);
        Assert.DoesNotContain("x:Name=\"DiagnosticsSectionRoot\"", source, StringComparison.Ordinal);
        Assert.DoesNotContain("OnSendClicked", source, StringComparison.Ordinal);
        Assert.DoesNotContain("OnCreateActiveLinkOfferClicked", source, StringComparison.Ordinal);
        Assert.DoesNotContain("OnConfirmContactVerificationClicked", source, StringComparison.Ordinal);
    }

    [Fact]
    public void UpdateIdentityHeader_DoesNotTriggerLifecycleRefetch()
    {
        string filePath = ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.Settings.cs");
        string source = File.ReadAllText(filePath);

        Assert.DoesNotContain("RefreshCurrentLifecycleContracts();", source, StringComparison.Ordinal);
    }

    [Fact]
    public void TestWindowAction_AppearsOnlyInDiagnosticsXaml()
    {
        string[] nonDiagnosticsFiles =
        [
            ResolveRepoPath("src", "Omni.Windows.App", "Features", "Onboarding", "OnboardingShellControl.xaml"),
            ResolveRepoPath("src", "Omni.Windows.App", "Features", "Settings", "SettingsSectionControl.xaml"),
            ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.xaml"),
        ];

        foreach (string filePath in nonDiagnosticsFiles)
        {
            string source = File.ReadAllText(filePath);
            Assert.DoesNotContain("Открыть отдельное тестовое окно", source, StringComparison.Ordinal);
            Assert.DoesNotContain("OpenIsolatedWindow", source, StringComparison.Ordinal);
            Assert.DoesNotContain("копию окна", source, StringComparison.Ordinal);
        }

        string diagnosticsSource = File.ReadAllText(
            ResolveRepoPath("src", "Omni.Windows.App", "Features", "Diagnostics", "DiagnosticsSectionControl.xaml"));
        Assert.DoesNotContain("Открыть отдельное тестовое окно", diagnosticsSource, StringComparison.Ordinal);
        Assert.Contains("DiagnosticsOpenIsolatedWindowButton", diagnosticsSource, StringComparison.Ordinal);
        Assert.Contains("Header=\"Второе окно\"", diagnosticsSource, StringComparison.Ordinal);
        Assert.DoesNotContain("DiagnosticsOpenIsolatedWindowButton\" Visibility=\"Collapsed\"", diagnosticsSource, StringComparison.Ordinal);
    }

    [Fact]
    public void UserFacingSectionXaml_DoesNotContain_RetiredEnglishLabels()
    {
        string[] files =
        [
            ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.xaml"),
            ResolveRepoPath("src", "Omni.Windows.App", "Features", "Onboarding", "OnboardingShellControl.xaml"),
            ResolveRepoPath("src", "Omni.Windows.App", "Features", "Chats", "ChatsSectionControl.xaml"),
            ResolveRepoPath("src", "Omni.Windows.App", "Features", "Contacts", "ContactsSectionControl.xaml"),
            ResolveRepoPath("src", "Omni.Windows.App", "Features", "Settings", "SettingsSectionControl.xaml"),
        ];
        string[] retiredPatterns =
        [
            "Content=\"Address book\"",
            "Text=\"Address book\"",
            "Content=\"Desktop\"",
            "Text=\"Desktop\"",
            "Content=\"Offer\"",
            "Text=\"Offer\"",
            "Content=\"Acceptance\"",
            "Text=\"Acceptance\"",
            "Content=\"Never met\"",
            "Text=\"Never met\"",
            "копию окна",
        ];

        foreach (string filePath in files)
        {
            string source = File.ReadAllText(filePath);
            foreach (string pattern in retiredPatterns)
            {
                Assert.DoesNotContain(pattern, source, StringComparison.Ordinal);
            }
        }
    }

    [Fact]
    public void MainSectionXaml_Declares_AdaptiveStates()
    {
        // Window-driven AdaptiveTrigger is correct for surfaces that span the whole
        // window (shell nav, onboarding) or only reflow cards (settings, diagnostics).
        string[] windowAdaptiveFiles =
        [
            ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.xaml"),
            ResolveRepoPath("src", "Omni.Windows.App", "Features", "Onboarding", "OnboardingShellControl.xaml"),
            ResolveRepoPath("src", "Omni.Windows.App", "Features", "Settings", "SettingsSectionControl.xaml"),
            ResolveRepoPath("src", "Omni.Windows.App", "Features", "Diagnostics", "DiagnosticsSectionControl.xaml"),
        ];

        foreach (string filePath in windowAdaptiveFiles)
        {
            string source = File.ReadAllText(filePath);
            Assert.Contains("VisualStateManager.VisualStateGroups", source, StringComparison.Ordinal);
            Assert.Contains("AdaptiveTrigger", source, StringComparison.Ordinal);
        }

        // Chats + Contacts are master/detail layouts hosted INSIDE the NavigationView
        // content, so they must react to their OWN width, not the window width:
        // AdaptiveTrigger.MinWindowWidth over-reports available space by the pane width
        // (up to 228px) and keeps the rail/primary columns too wide at half-screen.
        // They keep named visual states but switch them from code-behind on SizeChanged.
        // See docs/architecture/ui-audit.md §D.
        (string Xaml, string CodeBehind)[] controlWidthSections =
        [
            (ResolveRepoPath("src", "Omni.Windows.App", "Features", "Chats", "ChatsSectionControl.xaml"),
             ResolveRepoPath("src", "Omni.Windows.App", "Features", "Chats", "ChatsSectionControl.xaml.cs")),
            (ResolveRepoPath("src", "Omni.Windows.App", "Features", "Contacts", "ContactsSectionControl.xaml"),
             ResolveRepoPath("src", "Omni.Windows.App", "Features", "Contacts", "ContactsSectionControl.xaml.cs")),
        ];

        foreach ((string xamlPath, string codeBehindPath) in controlWidthSections)
        {
            string xaml = File.ReadAllText(xamlPath);
            Assert.Contains("VisualStateManager.VisualStateGroups", xaml, StringComparison.Ordinal);
            Assert.DoesNotContain("AdaptiveTrigger", xaml, StringComparison.Ordinal);

            string codeBehind = File.ReadAllText(codeBehindPath);
            Assert.Contains("SizeChanged", codeBehind, StringComparison.Ordinal);
            Assert.Contains("VisualStateManager.GoToState", codeBehind, StringComparison.Ordinal);
        }
    }

    [Fact]
    public void ContactsSection_Declares_MeshPairingControls_AndShellWiring()
    {
        string contactsXaml = File.ReadAllText(ResolveRepoPath(
            "src", "Omni.Windows.App", "Features", "Contacts", "ContactsSectionControl.xaml"));
        string accessorsSource = File.ReadAllText(
            ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.SectionAccessors.cs"));
        string eventsSource = File.ReadAllText(
            ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.SectionEvents.cs"));

        // The om3mesh pairing UI replaced the legacy om3 signaling block; guard
        // that it stays declared + wired (see docs/notes/legacy-signaling-ui-removal.md).
        foreach (string id in new[]
        {
            "GenerateMeshInviteButton", "GeneratedMeshInviteTextBox",
            "ImportMeshInviteTextBox", "PairOverMeshButton", "MeshPairingStatusTextBlock",
        })
        {
            Assert.Contains(id, contactsXaml, StringComparison.Ordinal);
            Assert.Contains(id, accessorsSource, StringComparison.Ordinal);
        }
        Assert.Contains("OnGenerateMeshInviteClicked", eventsSource, StringComparison.Ordinal);
        Assert.Contains("OnPairOverMeshClicked", eventsSource, StringComparison.Ordinal);

        // The legacy om3 signaling controls must be gone.
        Assert.DoesNotContain("GenerateSignalingBootstrapButton", contactsXaml, StringComparison.Ordinal);
        Assert.DoesNotContain("ConnectBySignalingButton", contactsXaml, StringComparison.Ordinal);
        Assert.DoesNotContain("SignalingInviteTokenCheckBox", contactsXaml, StringComparison.Ordinal);
    }

    [Fact]
    public void CallOverlay_IsFullWindowSurface_NotChatColumnCard()
    {
        string mainXaml = File.ReadAllText(ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.xaml"));
        string overlayXaml = File.ReadAllText(ResolveRepoPath(
            "src", "Omni.Windows.App", "Features", "Calls", "CallOverlayControl.xaml"));
        string chatsXaml = File.ReadAllText(ResolveRepoPath(
            "src", "Omni.Windows.App", "Features", "Chats", "ChatsSectionControl.xaml"));
        string accessorsSource = File.ReadAllText(
            ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.SectionAccessors.cs"));

        // The active call is a full-window overlay hosted by the shell root grid, not a
        // card wedged into the chat conversation column (docs/architecture/ui-audit.md #6).
        Assert.Contains("CallOverlayControl x:Name=\"CallOverlayHost\"", mainXaml, StringComparison.Ordinal);

        foreach (string id in new[]
        {
            "CallOverlayRoot", "RemoteCallVideoImage", "LocalCallVideoImage",
            "AcceptCurrentCallButton", "DeclineCurrentCallButton",
            "ToggleCurrentCallMuteButton", "ToggleCurrentCallMuteIcon", "HangUpCurrentCallButton",
        })
        {
            Assert.Contains(id, overlayXaml, StringComparison.Ordinal);
        }

        // Call elements now resolve from the overlay host, and the old chat-column
        // call card is gone.
        Assert.Contains("CallOverlayHost, nameof(RemoteCallVideoImage)", accessorsSource, StringComparison.Ordinal);
        Assert.Contains("CallOverlayHost, nameof(HangUpCurrentCallButton)", accessorsSource, StringComparison.Ordinal);
        Assert.DoesNotContain("ChatCallCardBorder", chatsXaml, StringComparison.Ordinal);
        Assert.DoesNotContain("RemoteCallVideoImage", chatsXaml, StringComparison.Ordinal);
    }

    [Fact]
    public void LegacyNetworkUi_IsGone_LocalStatusComesFromMesh_AndDiagnosticsPingStays()
    {
        string contactsXaml = File.ReadAllText(ResolveRepoPath(
            "src", "Omni.Windows.App", "Features", "Contacts", "ContactsSectionControl.xaml"));
        string settingsXaml = File.ReadAllText(ResolveRepoPath(
            "src", "Omni.Windows.App", "Features", "Settings", "SettingsSectionControl.xaml"));
        string diagnosticsXaml = File.ReadAllText(ResolveRepoPath(
            "src", "Omni.Windows.App", "Features", "Diagnostics", "DiagnosticsSectionControl.xaml"));
        string eventsSource = File.ReadAllText(ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.SectionEvents.cs"));
        string providersSource = File.ReadAllText(ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.RendezvousProviders.cs"));

        // The pre-mesh transport's surfaces are gone (docs/decisions/no-friend-tier.md, part 2): its
        // listener never runs, so "Сеть" / «Рядом» / «Подключиться» / LAN port / STUN list dialed or
        // configured nothing. LAN neighbours are found by mesh multicast on its own.
        Assert.DoesNotContain("Tag=\"network\"", contactsXaml, StringComparison.Ordinal);
        Assert.DoesNotContain("ContactsNetworkPanel", contactsXaml, StringComparison.Ordinal);
        Assert.DoesNotContain("LanPeersListView", contactsXaml, StringComparison.Ordinal);
        Assert.DoesNotContain("DiscoveryReceiveCheckBox", contactsXaml, StringComparison.Ordinal);
        Assert.DoesNotContain("DiscoveryPublishProfileComboBox", contactsXaml, StringComparison.Ordinal);
        Assert.DoesNotContain("SettingsTransportCard", settingsXaml, StringComparison.Ordinal);
        Assert.DoesNotContain("SettingsDiscoveryListenerSummaryTextBlock", settingsXaml, StringComparison.Ordinal);
        Assert.DoesNotContain("DiscoveryDiagnosticsCard", diagnosticsXaml, StringComparison.Ordinal);
        Assert.DoesNotContain("OnDiscoveredPeerConnectClicked", eventsSource, StringComparison.Ordinal);
        Assert.DoesNotContain("OnDiscoveryReceiveCheckBoxChanged", eventsSource, StringComparison.Ordinal);
        Assert.False(File.Exists(ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.Transport.ExplicitRoute.cs")));
        Assert.False(File.Exists(ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.StunSettings.cs")));

        // What stays: the courier card's local status line, now read from the mesh state rather
        // than the legacy discovery snapshot, and the diagnostics ping.
        Assert.Contains("SettingsSegmentLocalStatusTextBlock", settingsXaml, StringComparison.Ordinal);
        Assert.Contains("BuildMeshNodeStatusText()", providersSource, StringComparison.Ordinal);
        Assert.DoesNotContain("GetDiscoverySnapshot()", providersSource, StringComparison.Ordinal);
        Assert.Contains("DiagnosticsPingButton", diagnosticsXaml, StringComparison.Ordinal);
        Assert.Contains("DiagnosticsPingButton.Click += OnPingClicked;", eventsSource, StringComparison.Ordinal);
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
