using System;
using System.IO;
using System.Linq;
using System.Threading.Tasks;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;

namespace Omni.Windows.App;

// «Как вас находят» (Settings → "Сеть и обнаружение" group): the couriers — public pub/sub brokers (ntfy,
// Nostr relays) and the BitTorrent DHT — through which devices and contacts meet when there is no direct
// link. Distinct from relays (transport entry points). Each courier is "post and look", "only look" or
// "off" (a default one is turned off rather than removed; one the user or an invite added can be
// removed); each row says who sees what and what the courier did last. Mainline is one of the rows; its
// extra settings (entry node, segment bootstrap) sit below the list. See
// docs/decisions/courier-metadata-and-quota.md (step 6), global-identity-discovery.md.
public sealed partial class MainWindow
{
    private bool rendezvousProvidersAddWired;
    private bool dhtRendezvousToggleWired;
    private bool rendezvousVisibilityWired;
    private bool suppressDhtEntryNodeToggle;
    private bool suppressProviderModeChange;

    // The status line of each courier row, by courier id — refreshed in place by the live poll, so a
    // row (and its mode box) is not rebuilt under the user's hand every few seconds.
    private readonly System.Collections.Generic.Dictionary<long, TextBlock> providerStatusLines = new();

    // Live DHT-status poll: runs only while the Settings section is visible (started/stopped in
    // OnSettingsVisibilityChangedForRendezvous). Each tick reads dht_rendezvous_status OFF the UI thread
    // (a stuck helper must not freeze the poll) and updates the swarm-connection indicator.
    private Microsoft.UI.Dispatching.DispatcherQueueTimer? dhtStatusPollTimer;
    private bool dhtPollInFlight;

    private StackPanel SettingsProvidersListPanel =>
        ResolveSectionElement<StackPanel>(SettingsSectionHost, nameof(SettingsProvidersListPanel));

    private TextBlock SettingsDhtRendezvousStatusTextBlock =>
        ResolveSectionElement<TextBlock>(SettingsSectionHost, nameof(SettingsDhtRendezvousStatusTextBlock));

    private ToggleSwitch SettingsDhtEntryNodeToggle =>
        ResolveSectionElement<ToggleSwitch>(SettingsSectionHost, nameof(SettingsDhtEntryNodeToggle));

    private TextBlock SettingsDhtEntryNodeStatusTextBlock =>
        ResolveSectionElement<TextBlock>(SettingsSectionHost, nameof(SettingsDhtEntryNodeStatusTextBlock));

    private StackPanel SettingsDhtEntryNodeAddressPanel =>
        ResolveSectionElement<StackPanel>(SettingsSectionHost, nameof(SettingsDhtEntryNodeAddressPanel));

    private TextBlock SettingsDhtEntryNodeAddressTextBlock =>
        ResolveSectionElement<TextBlock>(SettingsSectionHost, nameof(SettingsDhtEntryNodeAddressTextBlock));

    private Button SettingsCopyDhtEntryNodeButton =>
        ResolveSectionElement<Button>(SettingsSectionHost, nameof(SettingsCopyDhtEntryNodeButton));

    private TextBlock SettingsRendezvousSegmentStatusTextBlock =>
        ResolveSectionElement<TextBlock>(SettingsSectionHost, nameof(SettingsRendezvousSegmentStatusTextBlock));

    private TextBlock SettingsSegmentLocalStatusTextBlock =>
        ResolveSectionElement<TextBlock>(SettingsSectionHost, nameof(SettingsSegmentLocalStatusTextBlock));

    private StackPanel SettingsDhtBootstrapListPanel =>
        ResolveSectionElement<StackPanel>(SettingsSectionHost, nameof(SettingsDhtBootstrapListPanel));

    private TextBox SettingsDhtBootstrapAddressTextBox =>
        ResolveSectionElement<TextBox>(SettingsSectionHost, nameof(SettingsDhtBootstrapAddressTextBox));

    private Button SettingsAddDhtBootstrapButton =>
        ResolveSectionElement<Button>(SettingsSectionHost, nameof(SettingsAddDhtBootstrapButton));

    private TextBlock SettingsDhtBootstrapStatusTextBlock =>
        ResolveSectionElement<TextBlock>(SettingsSectionHost, nameof(SettingsDhtBootstrapStatusTextBlock));

    private Button SettingsExportDhtBootstrapBundleButton =>
        ResolveSectionElement<Button>(SettingsSectionHost, nameof(SettingsExportDhtBootstrapBundleButton));

    private Button SettingsImportDhtBootstrapBundleButton =>
        ResolveSectionElement<Button>(SettingsSectionHost, nameof(SettingsImportDhtBootstrapBundleButton));

    private TextBox SettingsDhtBootstrapBundleTextBox =>
        ResolveSectionElement<TextBox>(SettingsSectionHost, nameof(SettingsDhtBootstrapBundleTextBox));

    private TextBlock SettingsDhtBootstrapBundleStatusTextBlock =>
        ResolveSectionElement<TextBlock>(SettingsSectionHost, nameof(SettingsDhtBootstrapBundleStatusTextBlock));

    private TextBox SettingsProviderUrlTextBox =>
        ResolveSectionElement<TextBox>(SettingsSectionHost, nameof(SettingsProviderUrlTextBox));

    private Button SettingsAddProviderButton =>
        ResolveSectionElement<Button>(SettingsSectionHost, nameof(SettingsAddProviderButton));

    private TextBlock SettingsProvidersStatusTextBlock =>
        ResolveSectionElement<TextBlock>(SettingsSectionHost, nameof(SettingsProvidersStatusTextBlock));

    private void InitializeRendezvousProviders()
    {
        if (!rendezvousProvidersAddWired)
        {
            SettingsAddProviderButton.Click += OnAddRendezvousProviderClicked;
            rendezvousProvidersAddWired = true;
        }

        if (!dhtRendezvousToggleWired)
        {
            SettingsDhtEntryNodeToggle.Toggled += OnDhtEntryNodeToggled;
            SettingsCopyDhtEntryNodeButton.Click += OnCopyDhtEntryNodeClicked;
            SettingsAddDhtBootstrapButton.Click += OnAddDhtBootstrapClicked;
            SettingsExportDhtBootstrapBundleButton.Click += OnExportDhtBootstrapBundleClicked;
            SettingsImportDhtBootstrapBundleButton.Click += OnImportDhtBootstrapBundleClicked;
            dhtRendezvousToggleWired = true;
        }

        if (!rendezvousVisibilityWired)
        {
            // This initializer runs once (post-sign-in), often before the Rust core handle is ready,
            // so the first read fails and the card shows its cold-load fallback. Re-read whenever the
            // Settings section becomes visible — by the time the user opens it the core is warm and the
            // list / DHT toggle / segment status all populate (same pattern as ContactsSectionControl).
            SettingsSectionRoot.RegisterPropertyChangedCallback(
                UIElement.VisibilityProperty, OnSettingsVisibilityChangedForRendezvous);
            rendezvousVisibilityWired = true;
        }

        RefreshRendezvousProviders();
        RefreshDhtBootstrap();
        RefreshDhtEntryNode();
    }

    private void OnSettingsVisibilityChangedForRendezvous(DependencyObject sender, DependencyProperty dp)
    {
        if (SettingsSectionRoot.Visibility != Visibility.Visible)
        {
            StopDhtStatusPolling();
            return;
        }

        DispatcherQueue.TryEnqueue(() =>
        {
            RefreshRendezvousProviders();
            RefreshDhtBootstrap();
            RefreshDhtEntryNode();
            // Re-read discoverability too: like the rendezvous card, the one post-sign-in read often
            // runs before the core handle is warm, so the Omni-ID box comes up empty and never refills.
            // By the time Settings is opened the core is warm — repopulate the copyable Omni-ID.
            RefreshDiscoverabilitySettings();
            StartDhtStatusPolling();
        });
    }

    // Whether the DHT helper can actually LAUNCH — either a sibling omni-dht-helper.exe ships next to
    // this build (multi-file / folder layout), OR this exe can self-exec it (single-file build, where
    // there is no sibling but Core/HelperMode routes `--run-helper dht` into the courier code; the Rust
    // spawn side takes the same path via app_rendezvous.rs::helper_launch). Checking only for the file
    // lied on single-file — it reported "rebuild the app" while self-exec would launch the leg fine.
    private static bool DhtHelperLaunchable() =>
        File.Exists(Path.Combine(AppContext.BaseDirectory, "omni-dht-helper.exe")) ||
        Core.HelperMode.SelfExecSupported;

    // Entry-node (server) role: show the toggle state + the public entry address to share. Applies on
    // the next per-profile rendezvous spawn (like the DHT toggle), so the address appears after the
    // engine (re)starts as an entry node.
    private void RefreshDhtEntryNode()
    {
        try
        {
            bool enabled = rustBridge.GetDhtEntryNodeEnabled();
            suppressDhtEntryNodeToggle = true;
            SettingsDhtEntryNodeToggle.IsOn = enabled;
            suppressDhtEntryNodeToggle = false;

            string endpoint = enabled ? rustBridge.GetDhtEntryNodeEndpoint() : string.Empty;
            bool hasEndpoint = !string.IsNullOrWhiteSpace(endpoint);
            SettingsDhtEntryNodeAddressPanel.Visibility =
                hasEndpoint ? Visibility.Visible : Visibility.Collapsed;
            if (hasEndpoint)
            {
                SettingsDhtEntryNodeAddressTextBlock.Text = endpoint;
            }

            if (!enabled)
            {
                SettingsDhtEntryNodeStatusTextBlock.Text = string.Empty;
            }
            else if (TryGetDhtStatus() is { Enabled: false })
            {
                SettingsDhtEntryNodeStatusTextBlock.Text =
                    "Включите Mainline в списке выше — без него движок не запускается, и точка входа не работает.";
            }
            else if (!DhtHelperLaunchable())
            {
                SettingsDhtEntryNodeStatusTextBlock.Text =
                    "DHT-хелпер недоступен (ни рядом файлом, ни через self-exec) — точка входа не поднимется.";
            }
            else if (hasEndpoint)
            {
                SettingsDhtEntryNodeStatusTextBlock.Text =
                    "Ваш узел — точка входа сегмента. Адрес ниже раздайте; после перезапуска порт может смениться.";
            }
            else
            {
                SettingsDhtEntryNodeStatusTextBlock.Text =
                    "Поднимаем точку входа — адрес появится через несколько секунд. Если не появится, роутер не пробросил порт (UPnP выключен или провайдерская сеть).";
            }
        }
        catch (Exception)
        {
            // Core not readable yet (pre-sign-in); leave as-is.
        }
    }

    private void OnDhtEntryNodeToggled(object sender, RoutedEventArgs e)
    {
        if (suppressDhtEntryNodeToggle)
        {
            return;
        }

        try
        {
            rustBridge.SetDhtEntryNodeEnabled(SettingsDhtEntryNodeToggle.IsOn);
        }
        catch (Exception ex)
        {
            SettingsDhtEntryNodeStatusTextBlock.Text = $"Не удалось изменить: {ex.Message}";
            suppressDhtEntryNodeToggle = true;
            SettingsDhtEntryNodeToggle.IsOn = !SettingsDhtEntryNodeToggle.IsOn;
            suppressDhtEntryNodeToggle = false;
            return;
        }

        RefreshDhtEntryNode();
        ApplyDhtToggleImmediately(); // respawn so the entry-node engine (re)starts now, not on next dial
    }

    private void OnCopyDhtEntryNodeClicked(object sender, RoutedEventArgs e)
    {
        string endpoint = (SettingsDhtEntryNodeAddressTextBlock.Text ?? string.Empty).Trim();
        if (endpoint.Length == 0)
        {
            return;
        }

        var package = new global::Windows.ApplicationModel.DataTransfer.DataPackage();
        package.SetText(endpoint);
        global::Windows.ApplicationModel.DataTransfer.Clipboard.SetContent(package);
        SettingsDhtEntryNodeStatusTextBlock.Text =
            "Адрес скопирован — друг добавит его у себя в «Узлы входа в рой».";
    }

    private void RefreshDhtBootstrap()
    {
        StackPanel list = SettingsDhtBootstrapListPanel;
        list.Children.Clear();

        System.Collections.Generic.IReadOnlyList<string> nodes;
        try
        {
            nodes = rustBridge.GetDhtBootstrapNodes();
        }
        catch (Exception)
        {
            return;
        }

        if (nodes.Count == 0)
        {
            list.Children.Add(new TextBlock
            {
                Text = "Список пуст — DHT входит в рой через глобальные роутеры (недоступны в изолированном сегменте).",
                Style = (Style)Application.Current.Resources["SettingCaptionTextStyle"],
                TextWrapping = TextWrapping.WrapWholeWords,
            });
            return;
        }

        foreach (string node in nodes)
        {
            list.Children.Add(BuildDhtBootstrapRow(node));
        }
    }

    private Border BuildDhtBootstrapRow(string address)
    {
        var grid = new Grid { ColumnSpacing = 12 };
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });

        var label = new TextBlock
        {
            Text = address,
            VerticalAlignment = VerticalAlignment.Center,
            FontFamily = new Microsoft.UI.Xaml.Media.FontFamily("Consolas"),
        };
        Grid.SetColumn(label, 0);
        grid.Children.Add(label);

        var remove = new Button { Content = "Удалить", VerticalAlignment = VerticalAlignment.Center };
        remove.Click += (_, _) => RemoveDhtBootstrapNode(address);
        Grid.SetColumn(remove, 1);
        grid.Children.Add(remove);

        return new Border
        {
            Style = (Style)Application.Current.Resources["SettingListRowStyle"],
            Child = grid,
        };
    }

    private void OnAddDhtBootstrapClicked(object sender, RoutedEventArgs e)
    {
        string addr = (SettingsDhtBootstrapAddressTextBox.Text ?? string.Empty).Trim();
        if (addr.Length == 0)
        {
            SettingsDhtBootstrapStatusTextBlock.Text = "Введите адрес DHT-узла внутри сегмента: ip:порт.";
            return;
        }

        try
        {
            rustBridge.AddDhtBootstrapNode(addr);
            SettingsDhtBootstrapAddressTextBox.Text = string.Empty;
            SettingsDhtBootstrapStatusTextBlock.Text =
                "Bootstrap-нода добавлена. Вступит в силу при следующем запуске профиля.";
        }
        catch (Exception ex)
        {
            SettingsDhtBootstrapStatusTextBlock.Text = $"Не удалось добавить: {ex.Message}";
        }

        RefreshDhtBootstrap();
    }

    private void RemoveDhtBootstrapNode(string address)
    {
        try
        {
            rustBridge.RemoveDhtBootstrapNode(address);
            SettingsDhtBootstrapStatusTextBlock.Text = "Bootstrap-нода удалена.";
        }
        catch (Exception ex)
        {
            SettingsDhtBootstrapStatusTextBlock.Text = $"Не удалось удалить: {ex.Message}";
        }

        RefreshDhtBootstrap();
    }

    // Export the configured bootstrap list as an `om3dhtboot:` token to the clipboard — the
    // contact-independent out-of-band seed channel (step A). See dht-bootstrap-mesh-propagation.md.
    private void OnExportDhtBootstrapBundleClicked(object sender, RoutedEventArgs e)
    {
        try
        {
            string token = rustBridge.ExportDhtBootstrapBundle();
            var package = new global::Windows.ApplicationModel.DataTransfer.DataPackage();
            package.SetText(token);
            global::Windows.ApplicationModel.DataTransfer.Clipboard.SetContent(package);
            SettingsDhtBootstrapBundleStatusTextBlock.Text =
                "Список скопирован как токен om3dhtboot:. Опубликуйте его внутри сегмента — импортировать сможет кто угодно.";
        }
        catch (Exception ex)
        {
            SettingsDhtBootstrapBundleStatusTextBlock.Text = $"Не удалось экспортировать: {ex.Message}";
        }
    }

    // Import a pasted `om3dhtboot:` token, merging its nodes into the configured list (step A).
    private void OnImportDhtBootstrapBundleClicked(object sender, RoutedEventArgs e)
    {
        string token = (SettingsDhtBootstrapBundleTextBox.Text ?? string.Empty).Trim();
        if (token.Length == 0)
        {
            SettingsDhtBootstrapBundleStatusTextBlock.Text = "Вставьте токен om3dhtboot:… для импорта.";
            return;
        }

        try
        {
            long added = rustBridge.ImportDhtBootstrapBundle(token);
            SettingsDhtBootstrapBundleTextBox.Text = string.Empty;
            SettingsDhtBootstrapBundleStatusTextBlock.Text = added > 0
                ? $"Импортировано новых узлов: {added}."
                : "Все узлы из бандла уже были в списке.";
        }
        catch (Exception ex)
        {
            SettingsDhtBootstrapBundleStatusTextBlock.Text = $"Не удалось импортировать: {ex.Message}";
        }

        RefreshDhtBootstrap();
    }

    // --- Live DHT-status poll + immediate apply -------------------------------------------------------

    /// Start (or resume) the 3s poll of dht_rendezvous_status while the Settings section is visible, and
    /// fire one poll right away so the indicator is current without waiting a full interval.
    private void StartDhtStatusPolling()
    {
        if (dhtStatusPollTimer is null)
        {
            dhtStatusPollTimer = DispatcherQueue.CreateTimer();
            dhtStatusPollTimer.Interval = TimeSpan.FromSeconds(3);
            dhtStatusPollTimer.Tick += (_, _) => PollDhtStatusOnce();
        }

        if (!dhtStatusPollTimer.IsRunning)
        {
            dhtStatusPollTimer.Start();
        }

        PollDhtStatusOnce();
    }

    private void StopDhtStatusPolling() => dhtStatusPollTimer?.Stop();

    /// One status read OFF the UI thread (the `nodes` IPC has a short timeout but must never block the UI),
    /// marshaled back to update the indicator. Guarded so a slow read can't pile up overlapping polls.
    private void PollDhtStatusOnce()
    {
        if (dhtPollInFlight)
        {
            return;
        }

        dhtPollInFlight = true;
        _ = Task.Run(() =>
        {
            RustBridge.DhtRendezvousStatus? status = TryGetDhtStatus();
            System.Collections.Generic.IReadOnlyList<Core.RendezvousProviderDto>? providers = null;
            try
            {
                providers = rustBridge.ListRendezvousProviders();
            }
            catch
            {
                // core not readable yet — keep the status lines as they are
            }

            DispatcherQueue.TryEnqueue(() =>
            {
                dhtPollInFlight = false;
                ApplyDhtLiveStatus(status);
                if (providers is not null)
                {
                    ApplyProviderStatuses(providers);
                }
            });
        });
    }

    /// Apply the DHT toggles IMMEDIATELY: respawn the rendezvous runtime (a child-process spawn — OFF the
    /// UI thread) so the helper starts now instead of on the next dial, then refresh the live status.
    private void ApplyDhtToggleImmediately()
    {
        StartDhtStatusPolling();
        _ = Task.Run(() =>
        {
            try
            {
                rustBridge.RespawnRendezvousRuntime();
            }
            catch
            {
                // Best-effort: the flag is already persisted, so it still applies on the next spawn.
            }

            RustBridge.DhtRendezvousStatus? status = TryGetDhtStatus();
            DispatcherQueue.TryEnqueue(() => ApplyDhtLiveStatus(status));
        });
    }

    /// The swarm answers within seconds when the network lets it through; half a minute with no node
    /// at all is a network that drops DHT traffic, not a slow join (the core's DHT_BLOCKED_AFTER_MS).
    private const long DhtBlockedAfterMs = 30_000;

    private RustBridge.DhtRendezvousStatus? TryGetDhtStatus()
    {
        try
        {
            return rustBridge.GetDhtRendezvousStatus();
        }
        catch
        {
            return null; // core not readable yet (pre-sign-in) — leave the current text
        }
    }

    private void ApplyDhtLiveStatus(RustBridge.DhtRendezvousStatus? status)
    {
        if (status is null)
        {
            return; // unknown — keep whatever the interim/refresh set
        }

        SettingsDhtRendezvousStatusTextBlock.Text =
            !status.Enabled ? "⚪ Mainline выключен — включите его в списке выше, чтобы настройки ниже работали."
            : !status.Running ? "🟡 Mainline запускается…"
            : status.NodeCount <= 0 && status.EmptyForMs >= DhtBlockedAfterMs ? DhtBlockedStatus(status.WayIn, status.FriendProxy)
            : status.NodeCount <= 0 ? "🟡 Подключаемся к рою Mainline…"
            : $"🟢 Подключено к рою Mainline · {status.NodeCount} {NodesWord(status.NodeCount)}";

        // Immediate-apply also makes the entry-node public address appear without a manual refresh —
        // flip the status text from "поднимается…" to ready and show the shareable address live.
        if (status.EntryNodeEnabled && !string.IsNullOrWhiteSpace(status.EntryEndpoint))
        {
            SettingsDhtEntryNodeAddressPanel.Visibility = Visibility.Visible;
            SettingsDhtEntryNodeAddressTextBlock.Text = status.EntryEndpoint;
            SettingsDhtEntryNodeStatusTextBlock.Text =
                "🟢 Ваш узел — точка входа сегмента. Адрес ниже раздайте; после перезапуска порт может смениться.";
        }
    }

    /// Our own Mainline sees no swarm: the network drops DHT traffic. The core then goes in through a
    /// friend's engine or the pkarr relays and says which way the last op went.
    private static string DhtBlockedStatus(string? wayIn, bool friendProxy) => wayIn switch
    {
        "friend" => "🟡 Сеть не пропускает Mainline — в рой идём через друга (его узел ищет за нас).",
        "pkarr relays" => "🟡 Сеть не пропускает Mainline — в рой идём через pkarr-relay (по HTTPS).",
        "none" => "🟠 Сеть не пропускает Mainline, и обходы (через друга, через pkarr-relay) сейчас не отвечают. Поиск идёт через другие курьеры (ntfy, Nostr).",
        // Not tried yet: the way in is taken by the next search for a contact (every few minutes).
        _ when friendProxy => "🟡 Сеть не пропускает Mainline — на связи друг, через которого можно ходить в рой; обход включится при следующем поиске контактов.",
        _ => "🟠 Сеть не пропускает Mainline — ни один узел роя не отвечает. Обход через pkarr-relay включится при следующем поиске контактов; пока ищем через ntfy и Nostr.",
    };

    /// Russian plural for "узел" (1 узел / 2–4 узла / 5+ узлов, with the 11–14 exception).
    private static string NodesWord(long count)
    {
        long mod100 = count % 100;
        long mod10 = count % 10;
        if (mod100 is >= 11 and <= 14)
        {
            return "узлов";
        }

        return mod10 switch
        {
            1 => "узел",
            >= 2 and <= 4 => "узла",
            _ => "узлов",
        };
    }

    private void RefreshRendezvousProviders()
    {
        StackPanel list = SettingsProvidersListPanel;
        list.Children.Clear();
        providerStatusLines.Clear();

        System.Collections.Generic.IReadOnlyList<Core.RendezvousProviderDto> providers;
        try
        {
            providers = rustBridge.ListRendezvousProviders();
        }
        catch (Exception)
        {
            // Core not readable yet (still initializing / pre-sign-in). The card repopulates when the
            // Settings section is shown again — surface that instead of a blank line.
            SettingsRendezvousSegmentStatusTextBlock.Text =
                "Список способов встречи появится после инициализации ядра.";
            return;
        }

        UpdateRendezvousSegmentStatus(providers);
        UpdateLocalConnectivityStatus();

        // What a kind of box costs is said once, on its first row: six Nostr relays repeated the same
        // paragraph six times (owner, 2026-10-13).
        var explained = new System.Collections.Generic.HashSet<string>();
        foreach (Core.RendezvousProviderDto provider in providers)
        {
            list.Children.Add(BuildRendezvousProviderRow(provider, explained.Add(provider.Kind)));
        }
    }

    // How many couriers this profile posts to and looks at, and how many of them live inside the local
    // segment (reachable when the outside internet is cut off).
    private void UpdateRendezvousSegmentStatus(
        System.Collections.Generic.IReadOnlyList<Core.RendezvousProviderDto> providers)
    {
        int posting = 0, reading = 0, local = 0;
        foreach (Core.RendezvousProviderDto provider in providers)
        {
            if (provider.Mode == "off")
            {
                continue;
            }

            if (provider.Mode == "publish_read")
            {
                posting++;
            }
            else
            {
                reading++;
            }

            if (IsLocalProviderUrl(provider.Url))
            {
                local++;
            }
        }

        SettingsRendezvousSegmentStatusTextBlock.Text = local == 0
            ? "Своего ящика в локальной сети нет — без внешнего интернета новых собеседников не встретить."
            : $"Ящиков в локальной сети: {local} — встреча работает и без внешнего интернета.";
    }

    // Honest local status from the mesh connection state (no new probe). LAN neighbours are found by
    // mesh multicast on its own; other subnets of a segment need a local entry node or a local
    // rendezvous provider — say so instead of a fabricated "you are isolated" verdict.
    private void UpdateLocalConnectivityStatus()
    {
        SettingsSegmentLocalStatusTextBlock.Text =
            $"Сейчас этот узел: {BuildMeshNodeStatusText()}.";
    }

    // A provider reachable only inside the local segment: a private/loopback/link-local IP host or a
    // .local name. Used only to label the segment status; parsing failures count as non-local.
    private static bool IsLocalProviderUrl(string url)
    {
        if (!Uri.TryCreate(url, UriKind.Absolute, out Uri? parsed))
        {
            return false;
        }

        string host = parsed.Host;
        if (host.EndsWith(".local", StringComparison.OrdinalIgnoreCase))
        {
            return true;
        }

        if (!System.Net.IPAddress.TryParse(host, out System.Net.IPAddress? ip))
        {
            return false;
        }

        if (System.Net.IPAddress.IsLoopback(ip))
        {
            return true;
        }

        byte[] b = ip.GetAddressBytes();
        if (b.Length != 4)
        {
            return false;
        }

        return b[0] == 10
            || (b[0] == 192 && b[1] == 168)
            || (b[0] == 172 && b[1] >= 16 && b[1] <= 31)
            || (b[0] == 169 && b[1] == 254);
    }

    private Border BuildRendezvousProviderRow(Core.RendezvousProviderDto provider, bool explainKind)
    {
        var grid = new Grid { ColumnSpacing = 12 };
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });

        var info = new StackPanel { Spacing = 2, VerticalAlignment = VerticalAlignment.Center };
        info.Children.Add(new TextBlock { Text = ProviderTitle(provider) });
        info.Children.Add(new TextBlock
        {
            Text = explainKind ? ProviderTradeOff(provider) : ProviderSourceLabel(provider.Source),
            Style = (Style)Application.Current.Resources["SettingCaptionTextStyle"],
            TextWrapping = TextWrapping.WrapWholeWords,
        });
        var status = new TextBlock
        {
            Text = ProviderStatusText(provider),
            Style = (Style)Application.Current.Resources["SettingCaptionTextStyle"],
            TextWrapping = TextWrapping.WrapWholeWords,
        };
        providerStatusLines[provider.Id] = status;
        info.Children.Add(status);
        Grid.SetColumn(info, 0);
        grid.Children.Add(info);

        var controls = new StackPanel { Spacing = 6, VerticalAlignment = VerticalAlignment.Center };
        var mode = new ComboBox { MinWidth = 200 };
        mode.Items.Add("Читать и публиковать");
        mode.Items.Add("Только читать");
        mode.Items.Add("Выключен");
        suppressProviderModeChange = true;
        mode.SelectedIndex = provider.Mode switch
        {
            "read" => 1,
            "off" => 2,
            _ => 0,
        };
        suppressProviderModeChange = false;
        long id = provider.Id;
        string current = provider.Mode;
        mode.SelectionChanged += (_, _) =>
        {
            if (suppressProviderModeChange)
            {
                return;
            }

            string chosen = mode.SelectedIndex switch
            {
                1 => "read",
                2 => "off",
                _ => "publish_read",
            };
            if (chosen != current)
            {
                SetRendezvousProviderMode(id, chosen);
            }
        };
        controls.Children.Add(mode);
        if (provider.Removable)
        {
            var remove = new Button { Content = "Удалить", HorizontalAlignment = HorizontalAlignment.Right };
            remove.Click += (_, _) => RemoveRendezvousProvider(id);
            controls.Children.Add(remove);
        }

        Grid.SetColumn(controls, 1);
        grid.Children.Add(controls);

        return new Border
        {
            Style = (Style)Application.Current.Resources["SettingListRowStyle"],
            Child = grid,
        };
    }

    // The row title: the host for a web courier, Mainline by name.
    private static string ProviderTitle(Core.RendezvousProviderDto provider)
    {
        if (provider.Kind == "dht")
        {
            return "Mainline (BitTorrent DHT)";
        }

        string host = Uri.TryCreate(provider.Url, UriKind.Absolute, out Uri? parsed) ? parsed.Authority : provider.Url;
        return provider.Kind == "ntfy" ? $"ntfy · {host}" : $"Nostr · {host}";
    }

    // Who sees what, and what the courier is good at — the consequence of keeping it, not its protocol.
    private static string ProviderTradeOff(Core.RendezvousProviderDto provider) => provider.Kind switch
    {
        "dht" => "Без владельца и без лимитов, работает, когда остальные недоступны. Ваш IP видят узлы роя, провайдер видит запросы. Без уведомлений: контакт находится за 1–2 минуты",
        "ntfy" => "Одна компания видит ваш IP и когда вы на связи. Мгновенные уведомления. Лимит — 250 публикаций в сутки на IP, общий со всеми за вашим роутером",
        _ when provider.Pooled => "Реле видит ваш IP, но только для части контактов: каждую пару обслуживают 2–3 реле из шести общих. Мгновенные уведомления",
        _ => "Реле видит ваш IP и всех, с кем вы встречаетесь через него. Мгновенные уведомления",
    };

    // What the courier did last, in the user's terms.
    private static string ProviderStatusText(Core.RendezvousProviderDto provider)
    {
        if (provider.Mode == "off")
        {
            return "⚪ Выключен";
        }

        string ago = provider.StatusAgeSecs is long secs ? $" · {AgoText(secs)}" : string.Empty;
        string detail = string.IsNullOrWhiteSpace(provider.StatusDetail) ? string.Empty : $": {provider.StatusDetail}";
        return provider.Status switch
        {
            "ok" => $"🟢 Работает{ago}",
            "quota" => $"🟠 Лимит на сегодня исчерпан — публикации идут через другие способы{ago}",
            "cooling" => $"🟡 Пауза после сбоя, скоро попробуем снова{ago}",
            "fail" => $"🔴 Не отвечает{detail}{ago}",
            _ => "⚪ Ещё не использовался",
        };
    }

    private static string AgoText(long secs) => secs switch
    {
        < 60 => "только что",
        < 3600 => $"{secs / 60} мин назад",
        _ => $"{secs / 3600} ч назад",
    };

    // Refresh each row's status line in place (the live poll).
    private void ApplyProviderStatuses(System.Collections.Generic.IReadOnlyList<Core.RendezvousProviderDto> providers)
    {
        foreach (Core.RendezvousProviderDto provider in providers)
        {
            if (providerStatusLines.TryGetValue(provider.Id, out TextBlock? line))
            {
                line.Text = ProviderStatusText(provider);
            }
        }
    }

    // Where this courier came from: a default, one the user added, or one a friend's pairing invite
    // contributed (so a first contact on a restrictive network converges on a working shared set).
    private static string ProviderSourceLabel(string source) => source switch
    {
        "base" => "по умолчанию",
        "invite" => "из приглашения друга",
        _ => "добавлен вами",
    };

    private void SetRendezvousProviderMode(long id, string mode)
    {
        try
        {
            rustBridge.SetRendezvousProviderMode(id, mode);
            SettingsProvidersStatusTextBlock.Text = mode switch
            {
                "off" => "Выключено — уже не используется.",
                "read" => "Теперь только ищем контакты через него, сами там не публикуемся.",
                _ => "Включено — публикуемся и ищем контакты через него.",
            };
        }
        catch (Exception ex)
        {
            SettingsProvidersStatusTextBlock.Text = $"Не удалось изменить: {ex.Message}";
        }

        RefreshRendezvousProviders();
        ApplyDhtToggleImmediately(); // the runtime restarted with the new set — bring it up now
    }

    private void OnAddRendezvousProviderClicked(object sender, RoutedEventArgs e)
    {
        string url = (SettingsProviderUrlTextBox.Text ?? string.Empty).Trim();
        if (url.Length == 0)
        {
            SettingsProvidersStatusTextBlock.Text =
                "Введите адрес: https://… для ntfy или wss://… для реле Nostr.";
            return;
        }

        try
        {
            rustBridge.AddRendezvousProvider(url);
            SettingsProviderUrlTextBox.Text = string.Empty;
            SettingsProvidersStatusTextBlock.Text = "Добавлено и уже используется.";
        }
        catch (Exception ex)
        {
            SettingsProvidersStatusTextBlock.Text = $"Не удалось добавить: {ex.Message}";
        }

        RefreshRendezvousProviders();
    }

    private void RemoveRendezvousProvider(long id)
    {
        try
        {
            rustBridge.RemoveRendezvousProvider(id);
            SettingsProvidersStatusTextBlock.Text = "Удалено.";
        }
        catch (Exception ex)
        {
            SettingsProvidersStatusTextBlock.Text = $"Не удалось удалить: {ex.Message}";
        }

        RefreshRendezvousProviders();
    }
}
