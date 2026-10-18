using System;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Windows.ApplicationModel.DataTransfer;

namespace Omni.Windows.App;

// «Узлы сети и релеи» (Settings → "Сеть и обнаружение" group): the user-managed registry of network
// entry nodes — one removable base node (the project VPS) + relays/bootstrap the user adds by ip:port.
// List + add + remove ANY node (incl. the base; the empty list still connects via LAN + invite). Mirrors
// the self-contained ConnectionPrivacy partial: resolves its own elements via the section accessor and
// drives the RustBridge network-node FFI, touching no other partial beyond one Initialize call site.
// Live per-node reachability is added in step 5 (when the mesh actually dials these entry points).
// See docs/notes/roadmap-CONTINUE.md (Always-connect epic).
public sealed partial class MainWindow
{
    private bool networkNodesAddWired;
    private bool networkNodesVisibilityWired;
    private string selfSeedAddresses = string.Empty;

    private StackPanel SettingsRelaysListPanel =>
        ResolveSectionElement<StackPanel>(SettingsSectionHost, nameof(SettingsRelaysListPanel));

    private TextBlock SettingsSelfSeedAddressTextBlock =>
        ResolveSectionElement<TextBlock>(SettingsSectionHost, nameof(SettingsSelfSeedAddressTextBlock));

    private Button SettingsCopySelfSeedButton =>
        ResolveSectionElement<Button>(SettingsSectionHost, nameof(SettingsCopySelfSeedButton));

    private TextBox SettingsRelayEndpointTextBox =>
        ResolveSectionElement<TextBox>(SettingsSectionHost, nameof(SettingsRelayEndpointTextBox));

    private TextBox SettingsRelayTcpEndpointTextBox =>
        ResolveSectionElement<TextBox>(SettingsSectionHost, nameof(SettingsRelayTcpEndpointTextBox));

    private TextBox SettingsRelayLabelTextBox =>
        ResolveSectionElement<TextBox>(SettingsSectionHost, nameof(SettingsRelayLabelTextBox));

    private Button SettingsAddRelayButton =>
        ResolveSectionElement<Button>(SettingsSectionHost, nameof(SettingsAddRelayButton));

    private TextBlock SettingsRelaysStatusTextBlock =>
        ResolveSectionElement<TextBlock>(SettingsSectionHost, nameof(SettingsRelaysStatusTextBlock));

    private void InitializeNetworkNodes()
    {
        if (!networkNodesAddWired)
        {
            SettingsAddRelayButton.Click += OnAddNetworkNodeClicked;
            SettingsCopySelfSeedButton.Click += OnCopySelfSeedClicked;
            networkNodesAddWired = true;
        }

        if (!networkNodesVisibilityWired)
        {
            // Runs once (post-sign-in), often before the core is warm — re-read on section-show so the
            // list + self-seed address populate reliably (same pattern as the rendezvous card).
            SettingsSectionRoot.RegisterPropertyChangedCallback(
                UIElement.VisibilityProperty, OnSettingsVisibilityChangedForNetworkNodes);
            networkNodesVisibilityWired = true;
        }

        RefreshNetworkNodes();
    }

    private void OnSettingsVisibilityChangedForNetworkNodes(DependencyObject sender, DependencyProperty dp)
    {
        if (SettingsSectionRoot.Visibility != Visibility.Visible)
        {
            return;
        }

        DispatcherQueue.TryEnqueue(RefreshNetworkNodes);
    }

    private void RefreshNetworkNodes()
    {
        StackPanel list = SettingsRelaysListPanel;
        list.Children.Clear();
        UpdateSelfSeedAddress();

        System.Collections.Generic.IReadOnlyList<Core.NetworkNodeDto> nodes;
        try
        {
            nodes = rustBridge.ListNetworkNodes();
        }
        catch (Exception)
        {
            // Best-effort: the core may not be readable yet (pre-sign-in). Leave the list empty.
            return;
        }

        if (nodes.Count == 0)
        {
            list.Children.Add(new TextBlock
            {
                Text = "Список пуст. Связь по локальной сети и приглашению работает и без узлов.",
                Style = (Style)Application.Current.Resources["SettingCaptionTextStyle"],
                TextWrapping = TextWrapping.WrapWholeWords,
            });
            return;
        }

        foreach (Core.NetworkNodeDto node in nodes)
        {
            list.Children.Add(BuildNetworkNodeRow(node));
        }
    }

    private Border BuildNetworkNodeRow(Core.NetworkNodeDto node)
    {
        var grid = new Grid { ColumnSpacing = 12 };
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });

        var info = new StackPanel { Spacing = 2, VerticalAlignment = VerticalAlignment.Center };
        info.Children.Add(new TextBlock { Text = node.Label });

        string where = node.Endpoint ?? node.NodeAddressHex ?? "адрес не задан";
        string sourceTag = node.Source == "base" ? "базовая нода" : "добавлен вручную";
        info.Children.Add(new TextBlock
        {
            Text = $"{where} · {sourceTag}",
            Style = (Style)Application.Current.Resources["SettingCaptionTextStyle"],
            TextWrapping = TextWrapping.WrapWholeWords,
        });
        info.Children.Add(new TextBlock
        {
            Text = NetworkNodeStatusLabel(node.Status),
            Style = (Style)Application.Current.Resources["SettingCaptionTextStyle"],
            TextWrapping = TextWrapping.WrapWholeWords,
        });
        long id = node.Id;
        if (node.BundleId is { Length: > 0 } bundleId)
        {
            // R1c inc.4: an IMPORTED relay from a friend's set — show which set + version (no "share"
            // checkbox: re-propagating a friend's relay is forbidden, first-party-only / no transitive).
            string shortId = bundleId.Length > 8 ? bundleId[..8] : bundleId;
            string version = node.BundleVersion is { } v ? $" · версия {v}" : string.Empty;
            info.Children.Add(new TextBlock
            {
                Text = $"Из набора друга {shortId}{version}",
                Style = (Style)Application.Current.Resources["SettingCaptionTextStyle"],
                TextWrapping = TextWrapping.WrapWholeWords,
            });
        }
        else
        {
            // R1c inc.3b: a relay I added — offer to share it as part of MY bundle, auto-propagated to
            // approved friends over the per-pair beacon. `.Click` fires on user toggle only (not the
            // programmatic IsChecked set).
            var share = new CheckBox
            {
                Content = "Поделиться с друзьями",
                IsChecked = node.Shared,
                Margin = new Thickness(0, 2, 0, 0),
            };
            share.Click += (s, _) => SetRelayShared(id, (s as CheckBox)?.IsChecked == true);
            info.Children.Add(share);
        }
        Grid.SetColumn(info, 0);
        grid.Children.Add(info);

        var remove = new Button { Content = "Удалить", VerticalAlignment = VerticalAlignment.Center };
        remove.Click += (_, _) => RemoveNetworkNode(id);
        Grid.SetColumn(remove, 1);
        grid.Children.Add(remove);

        return new Border
        {
            Style = (Style)Application.Current.Resources["SettingListRowStyle"],
            Child = grid,
        };
    }

    // Honest per-node reachability label from the Rust connection-state (Rust = truth): a pinned
    // relay is checked against the live mesh; a hint-only entry reads as an entry point until dialed.
    private static string NetworkNodeStatusLabel(string status) => status switch
    {
        "online" => "● В сети",
        "offline" => "○ Не дозвонился",
        "mesh_off" => "Сеть выключена",
        _ => "Точка входа (опознается при подключении)", // "hint" / unknown
    };

    private void OnAddNetworkNodeClicked(object sender, RoutedEventArgs e)
    {
        string endpoint = (SettingsRelayEndpointTextBox.Text ?? string.Empty).Trim();
        string tcpEndpoint = (SettingsRelayTcpEndpointTextBox.Text ?? string.Empty).Trim();
        string label = (SettingsRelayLabelTextBox.Text ?? string.Empty).Trim();
        if (endpoint.Length == 0)
        {
            SettingsRelaysStatusTextBlock.Text =
                "Введите IP:порт (например 203.0.113.7:443), узел@IP:порт, или вставьте om3relay:-набор релеев.";
            return;
        }

        // A pasted `om3relay:` bundle is a SIGNED SET of relays at once (R1c) — verify + import it via a
        // separate path; a plain `ip:port` is the single manual hint. One field, smart dispatch so the
        // user pastes either. See docs/decisions/r1c-rotatable-white-relay.md.
        if (endpoint.StartsWith("om3relay:", StringComparison.Ordinal))
        {
            try
            {
                long imported = rustBridge.ImportRelayBundle(endpoint);
                ClearRelayInputs();
                SettingsRelaysStatusTextBlock.Text = imported > 0
                    ? $"Импортировано релеев: {imported}."
                    : "Набор релеев не новее уже сохранённого — пропущено.";
            }
            catch (Exception ex)
            {
                SettingsRelaysStatusTextBlock.Text = $"Не удалось импортировать набор: {ex.Message}";
            }

            RefreshNetworkNodes();
            return;
        }

        // The optional TLS-over-TCP bridge rides as a "|tcp=<ip:port>" suffix the core parses
        // (R1a connection-quality ladder, rung-0 carrier). See connection-quality-ladder.md.
        string token = tcpEndpoint.Length == 0 ? endpoint : $"{endpoint}|tcp={tcpEndpoint}";

        try
        {
            rustBridge.AddNetworkNode(token, label);
            ClearRelayInputs();
            SettingsRelaysStatusTextBlock.Text = "Релей добавлен.";
        }
        catch (Exception ex)
        {
            SettingsRelaysStatusTextBlock.Text = $"Не удалось добавить: {ex.Message}";
        }

        RefreshNetworkNodes();
    }

    private void ClearRelayInputs()
    {
        SettingsRelayEndpointTextBox.Text = string.Empty;
        SettingsRelayTcpEndpointTextBox.Text = string.Empty;
        SettingsRelayLabelTextBox.Text = string.Empty;
    }

    // Peer-as-hub: this node's own LAN dial address(es) to hand a friend out-of-band so they add it
    // above as a segment entry point. Port = the TRANSPORT (QUIC) local port from the core — the same
    // socket generate_reach_token shares, i.e. what a same-segment peer actually dials (NOT the discovery
    // listener port). IPs = private RFC1918 IPv4 from the core's interface inventory (a pure C# read).
    private void UpdateSelfSeedAddress()
    {
        int port;
        try
        {
            string endpoint = rustBridge.GetLocalTransportEndpoint();
            int colon = endpoint.LastIndexOf(':');
            if (colon < 0 || !int.TryParse(endpoint[(colon + 1)..], out port) || port <= 0)
            {
                selfSeedAddresses = string.Empty;
                SettingsSelfSeedAddressTextBlock.Text = string.Empty;
                SettingsCopySelfSeedButton.IsEnabled = false;
                return;
            }
        }
        catch (Exception)
        {
            // Mesh not started / core not ready — no dialable transport port yet.
            selfSeedAddresses = string.Empty;
            SettingsSelfSeedAddressTextBlock.Text =
                "Локальный адрес появится, когда запустится сеть.";
            SettingsCopySelfSeedButton.IsEnabled = false;
            return;
        }

        var addresses = new System.Collections.Generic.List<string>();
        try
        {
            foreach (Core.NetworkInterfaceSnapshot iface in rustBridge.GetLocalNetworkInventory())
            {
                if (!iface.IsUp || iface.IsVirtual || iface.IsLoopback)
                {
                    continue;
                }

                foreach (string ip in iface.IPv4Addresses)
                {
                    if (IsPrivateIPv4(ip))
                    {
                        addresses.Add($"{ip}:{port}");
                    }
                }
            }
        }
        catch (Exception)
        {
            // Inventory read failed; fall through to the manual-IP hint.
        }

        if (addresses.Count == 0)
        {
            selfSeedAddresses = string.Empty;
            SettingsSelfSeedAddressTextBlock.Text =
                $"Транспорт слушает порт {port}. LAN-адрес не определён автоматически — дайте другу «ваш-LAN-IP:{port}» (IP: команда ipconfig).";
            SettingsCopySelfSeedButton.IsEnabled = false;
            return;
        }

        selfSeedAddresses = string.Join(Environment.NewLine, addresses);
        SettingsSelfSeedAddressTextBlock.Text = "Ваш локальный адрес для сегмента: " + string.Join(", ", addresses);
        SettingsCopySelfSeedButton.IsEnabled = true;
    }

    private void OnCopySelfSeedClicked(object sender, RoutedEventArgs e)
    {
        if (selfSeedAddresses.Length == 0)
        {
            return;
        }

        var package = new DataPackage();
        package.SetText(selfSeedAddresses);
        Clipboard.SetContent(package);
        SettingsRelaysStatusTextBlock.Text = "Локальный адрес скопирован. Передайте его другу вне сети.";
    }

    private static bool IsPrivateIPv4(string ip)
    {
        if (!System.Net.IPAddress.TryParse(ip, out System.Net.IPAddress? addr))
        {
            return false;
        }

        byte[] b = addr.GetAddressBytes();
        if (b.Length != 4)
        {
            return false;
        }

        return b[0] == 10
            || (b[0] == 192 && b[1] == 168)
            || (b[0] == 172 && b[1] >= 16 && b[1] <= 31)
            || (b[0] == 169 && b[1] == 254);
    }

    private void RemoveNetworkNode(long id)
    {
        try
        {
            rustBridge.RemoveNetworkNode(id);
            SettingsRelaysStatusTextBlock.Text = "Узел удалён.";
        }
        catch (Exception ex)
        {
            SettingsRelaysStatusTextBlock.Text = $"Не удалось удалить: {ex.Message}";
        }

        RefreshNetworkNodes();
    }

    // R1c inc.3b: flag/unflag a relay as part of MY bundle, auto-propagated to approved friends over
    // the per-pair beacon (the core bumps my bundle version so the rotation propagates).
    private void SetRelayShared(long id, bool shared)
    {
        try
        {
            rustBridge.SetRelayShared(id, shared);
            SettingsRelaysStatusTextBlock.Text = shared
                ? "Релей будет предлагаться друзьям."
                : "Релей больше не предлагается друзьям.";
        }
        catch (Exception ex)
        {
            SettingsRelaysStatusTextBlock.Text = $"Не удалось изменить: {ex.Message}";
        }

        RefreshNetworkNodes();
    }
}
