using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;

namespace Omni.Windows.App;

// Each page of Settings reads what it shows when it comes on screen. Reading everything on every visit
// held the switch to Settings 130-320 ms, nearly all of it for pages nobody had open. The reads wait
// for the page to be drawn and run one at a time, so the window can draw between them; a page left
// meanwhile skips what remains.
public sealed partial class MainWindow
{
    private void InitializeSettingsPages()
    {
        SettingsSectionRoot.RegisterPropertyChangedCallback(UIElement.VisibilityProperty, (_, _) => OnSettingsPageChanged());
        SettingsSectionHost.SectionShown += OnSettingsPageChanged;
    }

    private void OnSettingsPageChanged()
    {
        bool shown = SettingsSectionRoot.Visibility == Visibility.Visible;
        string page = SettingsSectionHost.CurrentSection;

        if (shown && page == "network")
        {
            pinnedRelayTimer?.Start();
        }
        else
        {
            pinnedRelayTimer?.Stop();
        }

        if (!shown || page is not ("network" or "advanced"))
        {
            StopDhtStatusPolling();
        }

        if (!shown)
        {
            return;
        }

        // The second frame from here is the one the page was laid out and drawn in.
        int frames = 0;
        EventHandler<object>? onFrame = null;
        onFrame = (_, _) =>
        {
            if (++frames == 2)
            {
                Microsoft.UI.Xaml.Media.CompositionTarget.Rendering -= onFrame;
                ReadSettingsPage(page);
            }
        };
        Microsoft.UI.Xaml.Media.CompositionTarget.Rendering += onFrame;
    }

    private void ReadSettingsPage(string page)
    {
        switch (page)
        {
            case "profile":
                ReadForPage(page, "settings.backup_reminder", RefreshBackupReminder);
                break;
            case "privacy":
                ReadForPage(page, "settings.discoverability", RefreshDiscoverabilitySettings);
                ReadForPage(page, "settings.publish_plans", RefreshPublishPlansAsync);
                break;
            case "network":
                ReadForPage(page, "settings.rendezvous", RefreshRendezvousProviders);
                ReadForPage(page, "settings.network_nodes", RefreshNetworkNodes);
                ReadForPage(page, "settings.publish_plans", RefreshPublishPlansAsync);
                ReadForPage(page, "settings.public_relay_role", LoadPublicRelayRole);
                ReadForPage(page, "settings.pinned_relay", LoadPinnedRelayAsync);
                ReadForPage(page, "settings.dht_status", StartDhtStatusPolling);
                break;
            case "calls":
                // The capability probe (MF/QSV/NVENC) and camera enumeration (MF sensor stack) stay off
                // the boot path until somebody opens this page; the audio list is re-read each time, for
                // a headset plugged in while the window was open.
                ReadForPage(page, "settings.device_pickers", EnsureCallVideoDevicePickersPopulated);
                ReadForPage(page, "settings.camera_picker", EnsureCameraPickerPopulated);
                ReadForPage(page, "settings.audio_devices", RefreshAudioDevicePickers);
                break;
            case "advanced":
                ReadForPage(page, "settings.congestion", () => RefreshCongestionStatusText(LoadPersistedCongestionMode()));
                ReadForPage(page, "settings.rendezvous", RefreshRendezvousProviders);
                ReadForPage(page, "settings.dht_bootstrap", RefreshDhtBootstrap);
                ReadForPage(page, "settings.dht_entry_node", RefreshDhtEntryNode);
                ReadForPage(page, "settings.self_seed", UpdateSelfSeedAddress);
                ReadForPage(page, "settings.dht_status", StartDhtStatusPolling);
                break;
        }
    }

    private void ReadForPage(string page, string operation, System.Action read)
        => DispatcherQueue.TryEnqueue(DispatcherQueuePriority.Low, () =>
        {
            if (SettingsSectionRoot.Visibility == Visibility.Visible && SettingsSectionHost.CurrentSection == page)
            {
                TryRefresh(operation, read);
            }
        });
}
