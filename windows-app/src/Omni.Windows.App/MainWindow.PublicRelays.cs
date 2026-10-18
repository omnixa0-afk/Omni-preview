using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;

namespace Omni.Windows.App;

// «Публичные релеи» (Settings → «Связь»): the directory of relays their owners opened to everyone
// (docs/decisions/public-relay-directory.md). Search only when asked, filter and sort the loaded list
// here, «В запасные» pins one into «Узлы сети и релеи» by its node, «Сидеть через него» makes the profile
// go only through it (stage 3; the banner above the list says through whom and whether it holds). Plus
// the operator's own switch «Быть релеем для всех».
public sealed partial class MainWindow
{
    private bool publicRelaysWired;
    private IReadOnlyList<Core.PublicRelay> foundPublicRelays = Array.Empty<Core.PublicRelay>();
    private bool publicRelayRoleLoading;
    private Core.PinnedRelayState? pinnedRelayState;
    private bool pinnedRelayLoading;
    private DispatcherTimer? pinnedRelayTimer;

    private Button SettingsFindRelaysButton => ResolveSectionElement<Button>(SettingsSectionHost, nameof(SettingsFindRelaysButton));
    private TextBox SettingsRelayFilterTextBox => ResolveSectionElement<TextBox>(SettingsSectionHost, nameof(SettingsRelayFilterTextBox));
    private ComboBox SettingsRelaySortComboBox => ResolveSectionElement<ComboBox>(SettingsSectionHost, nameof(SettingsRelaySortComboBox));
    private TextBlock SettingsFindRelaysStatusTextBlock => ResolveSectionElement<TextBlock>(SettingsSectionHost, nameof(SettingsFindRelaysStatusTextBlock));
    private StackPanel SettingsPublicRelaysListPanel => ResolveSectionElement<StackPanel>(SettingsSectionHost, nameof(SettingsPublicRelaysListPanel));
    private ToggleSwitch SettingsPublicRelayToggle => ResolveSectionElement<ToggleSwitch>(SettingsSectionHost, nameof(SettingsPublicRelayToggle));
    private TextBox SettingsPublicRelayNameTextBox => ResolveSectionElement<TextBox>(SettingsSectionHost, nameof(SettingsPublicRelayNameTextBox));
    private TextBlock SettingsPublicRelayStatusTextBlock => ResolveSectionElement<TextBlock>(SettingsSectionHost, nameof(SettingsPublicRelayStatusTextBlock));
    private InfoBar SettingsPinnedRelayInfoBar => ResolveSectionElement<InfoBar>(SettingsSectionHost, nameof(SettingsPinnedRelayInfoBar));
    private Button SettingsUnpinRelayButton => ResolveSectionElement<Button>(SettingsSectionHost, nameof(SettingsUnpinRelayButton));
    private CheckBox SettingsPinnedRelayHiddenCheckBox => ResolveSectionElement<CheckBox>(SettingsSectionHost, nameof(SettingsPinnedRelayHiddenCheckBox));

    private void InitializePublicRelays()
    {
        if (!publicRelaysWired)
        {
            SettingsFindRelaysButton.Click += async (_, _) => await FindPublicRelaysAsync();
            SettingsRelayFilterTextBox.TextChanged += (_, _) => RenderPublicRelays();
            SettingsRelaySortComboBox.SelectionChanged += (_, _) => RenderPublicRelays();
            SettingsPublicRelayToggle.Toggled += (_, _) => ApplyPublicRelayRole();
            SettingsPublicRelayNameTextBox.LostFocus += (_, _) =>
            {
                if (SettingsPublicRelayToggle.IsOn)
                {
                    ApplyPublicRelayRole();
                }
            };
            SettingsUnpinRelayButton.Click += (_, _) => UnpinRelay();
            SettingsPinnedRelayHiddenCheckBox.Checked += (_, _) => ApplyPinnedRelayHidden();
            SettingsPinnedRelayHiddenCheckBox.Unchecked += (_, _) => ApplyPinnedRelayHidden();
            // Whether the relay holds changes on its own: re-read while the settings are on screen.
            pinnedRelayTimer = new DispatcherTimer { Interval = TimeSpan.FromSeconds(2) };
            pinnedRelayTimer.Tick += (_, _) => LoadPinnedRelay();
            SettingsSectionRoot.RegisterPropertyChangedCallback(UIElement.VisibilityProperty, (_, _) =>
            {
                if (SettingsSectionRoot.Visibility == Visibility.Visible)
                {
                    DispatcherQueue.TryEnqueue(LoadPublicRelayRole);
                    DispatcherQueue.TryEnqueue(LoadPinnedRelay);
                    pinnedRelayTimer.Start();
                }
                else
                {
                    pinnedRelayTimer.Stop();
                }
            });
            publicRelaysWired = true;
        }

        LoadPublicRelayRole();
        LoadPinnedRelay();
        if (SettingsSectionRoot.Visibility == Visibility.Visible)
        {
            pinnedRelayTimer?.Start();
        }
    }

    private void LoadPinnedRelay()
    {
        Core.PinnedRelayState state;
        try
        {
            state = rustBridge.GetPinnedRelay();
        }
        catch (Exception)
        {
            // Before sign-in there is no profile; the section re-reads when shown.
            return;
        }

        bool pinChanged = pinnedRelayState?.Relay?.NodeId != state.Relay?.NodeId;
        pinnedRelayState = state;
        RenderPinnedRelay(state);
        if (pinChanged)
        {
            RenderPublicRelays();
        }
    }

    private void RenderPinnedRelay(Core.PinnedRelayState state)
    {
        InfoBar bar = SettingsPinnedRelayInfoBar;
        if (state.Relay is not { } relay)
        {
            bar.IsOpen = false;
            return;
        }

        pinnedRelayLoading = true;
        SettingsPinnedRelayHiddenCheckBox.IsChecked = relay.KeepAddressHidden;
        pinnedRelayLoading = false;
        bar.Title = $"Через релей «{RelayDisplayName(relay.Name, relay.NodeId)}»" + (relay.Always ? "" : " — до закрытия программы");
        (bar.Severity, bar.Message) = Presentation.MainWindowPresentation.DescribePinnedRelay(state.Linked, state.LostSecs, relay.KeepAddressHidden, relay.Always);
        bar.IsOpen = true;
    }

    private void PinRelay(Core.PublicRelay relay, bool always)
    {
        try
        {
            rustBridge.PinRelay(new Core.PinnedRelay(
                relay.NodeId,
                relay.Name,
                relay.Udp[0],
                relay.Tcp.Count > 0 ? relay.Tcp[0] : null,
                always,
                pinnedRelayState?.Relay?.KeepAddressHidden ?? false));
            SettingsFindRelaysStatusTextBlock.Text = string.Empty;
        }
        catch (Exception ex)
        {
            SettingsFindRelaysStatusTextBlock.Text = $"Не удалось: {ex.Message}";
        }

        LoadPinnedRelay();
    }

    private void UnpinRelay()
    {
        try
        {
            rustBridge.UnpinRelay();
        }
        catch (Exception ex)
        {
            SettingsFindRelaysStatusTextBlock.Text = $"Не удалось: {ex.Message}";
        }

        LoadPinnedRelay();
    }

    private void ApplyPinnedRelayHidden()
    {
        if (pinnedRelayLoading || pinnedRelayState?.Relay is not { } relay)
        {
            return;
        }

        try
        {
            rustBridge.PinRelay(relay with { KeepAddressHidden = SettingsPinnedRelayHiddenCheckBox.IsChecked == true });
        }
        catch (Exception ex)
        {
            SettingsFindRelaysStatusTextBlock.Text = $"Не удалось: {ex.Message}";
        }

        LoadPinnedRelay();
    }

    private void LoadPublicRelayRole()
    {
        try
        {
            Core.PublicRelayRole role = rustBridge.GetPublicRelayRole();
            publicRelayRoleLoading = true;
            SettingsPublicRelayToggle.IsOn = role.On;
            SettingsPublicRelayNameTextBox.Text = role.Name;
            publicRelayRoleLoading = false;
            SettingsPublicRelayStatusTextBlock.Text = DescribePublicRelayRole(role.On);
        }
        catch (Exception)
        {
            // Before sign-in the core has no profile to read; the section re-reads when shown.
            publicRelayRoleLoading = false;
        }
    }

    private void ApplyPublicRelayRole()
    {
        if (publicRelayRoleLoading)
        {
            return;
        }

        bool on = SettingsPublicRelayToggle.IsOn;
        try
        {
            rustBridge.SetPublicRelayRole(on, (SettingsPublicRelayNameTextBox.Text ?? string.Empty).Trim());
            SettingsPublicRelayStatusTextBlock.Text = DescribePublicRelayRole(on);
        }
        catch (Exception ex)
        {
            SettingsPublicRelayStatusTextBlock.Text = $"Не удалось: {ex.Message}";
        }
    }

    // The entry goes out only from a node reachable from outside (core: public endpoints only) — say so
    // instead of letting the switch look like it works.
    private string DescribePublicRelayRole(bool on)
    {
        if (!on)
        {
            return "Выключено.";
        }

        bool reachable;
        try
        {
            reachable = rustBridge.MeshConnectionState().Node.Kind == "online_reachable";
        }
        catch (Exception)
        {
            reachable = false;
        }

        return reachable
            ? "Включено: запись появится в списке в течение пары минут и обновляется каждые 15 минут."
            : "Включено, но ваш адрес сейчас недоступен снаружи — пока так, в список вы не попадёте.";
    }

    private async Task FindPublicRelaysAsync()
    {
        SettingsFindRelaysButton.IsEnabled = false;
        SettingsFindRelaysStatusTextBlock.Text = "Ищу в почтовых ящиках…";
        try
        {
            Core.RelaySearchResult result = await Task.Run(() => rustBridge.SearchPublicRelays());
            foundPublicRelays = result.Relays;
            string answered = result.Answered.Count == 0
                ? string.Empty
                : " Ответили: " + string.Join(", ", result.Answered.Select(CourierKindName)) + ".";
            SettingsFindRelaysStatusTextBlock.Text = result.Status switch
            {
                "found" => $"Найдено: {result.Relays.Count}.{answered}",
                "nobody" => $"Публичных релеев сейчас нет.{answered}",
                _ => "Ни один почтовый ящик не ответил — проверьте связь и попробуйте ещё раз.",
            };
        }
        catch (Exception ex)
        {
            foundPublicRelays = Array.Empty<Core.PublicRelay>();
            SettingsFindRelaysStatusTextBlock.Text = $"Не удалось найти: {ex.Message}";
        }
        finally
        {
            SettingsFindRelaysButton.IsEnabled = true;
        }

        RenderPublicRelays();
    }

    private void RenderPublicRelays()
    {
        StackPanel list = SettingsPublicRelaysListPanel;
        list.Children.Clear();
        string filter = (SettingsRelayFilterTextBox.Text ?? string.Empty).Trim();
        IEnumerable<Core.PublicRelay> shown = foundPublicRelays.Where(relay =>
            filter.Length == 0 ||
            relay.Name.Contains(filter, StringComparison.CurrentCultureIgnoreCase) ||
            relay.NodeId.Contains(filter, StringComparison.OrdinalIgnoreCase));
        string sort = (SettingsRelaySortComboBox.SelectedItem as ComboBoxItem)?.Tag as string ?? "score";
        shown = sort switch
        {
            "seniority" => shown.OrderByDescending(relay => relay.SenioritySecs),
            "name" => shown.OrderBy(relay => relay.Name.Length == 0 ? 1 : 0)
                .ThenBy(relay => relay.Name, StringComparer.CurrentCultureIgnoreCase),
            "id" => shown.OrderBy(relay => relay.NodeId, StringComparer.Ordinal),
            _ => shown, // the core's order is the composite score
        };
        foreach (Core.PublicRelay relay in shown)
        {
            list.Children.Add(BuildPublicRelayRow(relay));
        }

        if (list.Children.Count == 0 && foundPublicRelays.Count > 0)
        {
            list.Children.Add(new TextBlock
            {
                Text = "Под фильтр ничего не подходит.",
                Style = (Style)Application.Current.Resources["SettingCaptionTextStyle"],
            });
        }
    }

    private Border BuildPublicRelayRow(Core.PublicRelay relay)
    {
        var grid = new Grid { ColumnSpacing = 12 };
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });

        var info = new StackPanel { Spacing = 2, VerticalAlignment = VerticalAlignment.Center };
        // The name is the operator's own words; the id beside it is what cannot be faked (owner, 2026-10-14).
        string name = relay.Name.Length == 0 ? "Без названия" : relay.Name;
        info.Children.Add(new TextBlock { Text = $"{name}   {ShortNodeId(relay.NodeId)}", TextWrapping = TextWrapping.WrapWholeWords });
        info.Children.Add(new TextBlock
        {
            Text = $"Стаж {FormatSeniority(relay.SenioritySecs)} · {LoadWords(relay.Load)} · найден: " +
                   string.Join(", ", relay.FoundOn.Select(CourierKindName)),
            Style = (Style)Application.Current.Resources["SettingCaptionTextStyle"],
            TextWrapping = TextWrapping.WrapWholeWords,
        });
        ToolTipService.SetToolTip(info, string.Join("\n", relay.Udp.Concat(relay.Tcp.Select(tcp => $"{tcp} (TCP)")).Prepend(relay.NodeId)));
        Grid.SetColumn(info, 0);
        grid.Children.Add(info);

        // A pinned node needs its UDP address (the TCP door rides along); a TCP-only relay cannot be one yet.
        var spare = new Button
        {
            Content = "В запасные",
            VerticalAlignment = VerticalAlignment.Center,
            IsEnabled = relay.Udp.Count > 0,
        };
        ToolTipService.SetToolTip(spare, "Добавить в «Узлы сети и релеи»: через него пойдёт связь, когда напрямую не выйдет.");
        spare.Click += (_, _) => AddPublicRelayAsSpare(relay);

        var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, VerticalAlignment = VerticalAlignment.Center };
        buttons.Children.Add(spare);
        buttons.Children.Add(BuildPinRelayButton(relay));
        Grid.SetColumn(buttons, 1);
        grid.Children.Add(buttons);

        return new Border
        {
            Style = (Style)Application.Current.Resources["SettingListRowStyle"],
            Child = grid,
        };
    }

    // «Сидеть через него ▾» — until the app closes, or always. The relay already pinned says so instead.
    private Control BuildPinRelayButton(Core.PublicRelay relay)
    {
        if (string.Equals(pinnedRelayState?.Relay?.NodeId, relay.NodeId, StringComparison.OrdinalIgnoreCase))
        {
            return new Button { Content = "Сижу через него", IsEnabled = false, VerticalAlignment = VerticalAlignment.Center };
        }

        var menu = new MenuFlyout { Placement = Microsoft.UI.Xaml.Controls.Primitives.FlyoutPlacementMode.BottomEdgeAlignedRight };
        var untilClose = new MenuFlyoutItem { Text = "До закрытия программы" };
        untilClose.Click += (_, _) => PinRelay(relay, always: false);
        var always = new MenuFlyoutItem { Text = "Всегда" };
        always.Click += (_, _) => PinRelay(relay, always: true);
        menu.Items.Add(untilClose);
        menu.Items.Add(always);
        var pin = new DropDownButton
        {
            Content = "Сидеть через него",
            Flyout = menu,
            VerticalAlignment = VerticalAlignment.Center,
            IsEnabled = relay.Udp.Count > 0,
        };
        ToolTipService.SetToolTip(pin, "Вся связь пойдёт только через этот релей, напрямую — нет: собеседники не увидят ваш адрес.");
        AutomationProperties.SetAutomationId(pin, "PinRelay_" + relay.NodeId);
        return pin;
    }

    private static string RelayDisplayName(string name, string nodeId)
        => name.Length == 0 ? ShortNodeId(nodeId) : name;

    private void AddPublicRelayAsSpare(Core.PublicRelay relay)
    {
        string token = $"{relay.NodeId}@{relay.Udp[0]}";
        if (relay.Tcp.Count > 0)
        {
            token += $"|tcp={relay.Tcp[0]}";
        }

        try
        {
            rustBridge.AddNetworkNode(token, relay.Name.Length == 0 ? $"Публичный релей {ShortNodeId(relay.NodeId)}" : relay.Name);
            SettingsFindRelaysStatusTextBlock.Text = $"«{(relay.Name.Length == 0 ? ShortNodeId(relay.NodeId) : relay.Name)}» добавлен в запасные.";
        }
        catch (Exception ex)
        {
            SettingsFindRelaysStatusTextBlock.Text = $"Не удалось добавить: {ex.Message}";
        }

        RefreshNetworkNodes();
    }

    private static string ShortNodeId(string nodeId)
        => nodeId.Length > 10 ? $"{nodeId[..4]}…{nodeId[^4..]}" : nodeId;

    private static string FormatSeniority(long secs) => secs switch
    {
        < 3600 => "меньше часа",
        < 86_400 => $"{secs / 3600} ч",
        _ => $"{secs / 86_400} дн",
    };

    private static string LoadWords(string load) => load switch
    {
        "busy" => "занят",
        "full" => "почти полон",
        _ => "свободен",
    };

    private static string CourierKindName(string kind) => kind switch
    {
        "nostr" => "Nostr",
        "ntfy" => "ntfy",
        "dht" => "Mainline",
        _ => kind,
    };
}
