using System;
using System.Threading.Tasks;
using Microsoft.UI.Xaml.Controls;

namespace Omni.Windows.App;

// Carrier picker (Settings → "Для опытных"): the carrier of OUR hop — from this node to the next one,
// a relay or the contact itself (owner, 2026-10-11: «A auto/tcp/quic B»); what the next node does
// onwards is its own setting. Авто (QUIC where it works, TCP where it does not) vs TCP (the relay
// through its TCP bridge, a contact through its TCP door — survives a UDP-hostile network) vs QUIC
// (UDP only — fastest, dead where UDP is blocked). Self-contained
// partial mirroring MainWindow.ConnectionPrivacy: resolves its own elements via the section accessor and
// drives the RustBridge carrier-policy FFI, touching no other partial beyond one Initialize call site.
// See docs/decisions/connection-quality-ladder.md.
public sealed partial class MainWindow
{
    // Guards SelectionChanged while we set SelectedIndex programmatically (mirrors the privacy picker),
    // so populating the picker doesn't write the policy back.
    private bool suppressCarrierPolicySelection;

    private ComboBox SettingsCarrierPolicyComboBox =>
        ResolveSectionElement<ComboBox>(SettingsSectionHost, nameof(SettingsCarrierPolicyComboBox));

    private TextBlock SettingsCarrierPolicySummaryTextBlock =>
        ResolveSectionElement<TextBlock>(
            SettingsSectionHost,
            nameof(SettingsCarrierPolicySummaryTextBlock));

    private void InitializeCarrierPolicyPicker()
    {
        ComboBox combo = SettingsCarrierPolicyComboBox;
        if (combo.Items.Count == 0)
        {
            combo.Items.Add(new ComboBoxItem
            {
                Content = "Авто — QUIC, где проходит, иначе TCP",
                Tag = "auto",
            });
            combo.Items.Add(new ComboBoxItem
            {
                Content = "TCP — надёжно, работает и без UDP",
                Tag = "tcp",
            });
            combo.Items.Add(new ComboBoxItem
            {
                Content = "QUIC — быстрее всего, нужен UDP",
                Tag = "quic",
            });
            combo.SelectionChanged += OnCarrierPolicySelectionChanged;
        }

        RefreshCarrierPolicySelection();
    }

    private void RefreshCarrierPolicySelection()
    {
        string policy;
        try
        {
            policy = rustBridge.GetCarrierPolicy();
        }
        catch (Exception)
        {
            policy = "auto"; // best-effort: fall back to the default if the core can't be read yet
        }

        suppressCarrierPolicySelection = true;
        try
        {
            ComboBox combo = SettingsCarrierPolicyComboBox;
            int index = 0;
            for (int i = 0; i < combo.Items.Count; i++)
            {
                if (combo.Items[i] is ComboBoxItem item && item.Tag as string == NormalizeCarrierPolicy(policy))
                {
                    index = i;
                    break;
                }
            }

            combo.SelectedIndex = index;
        }
        finally
        {
            suppressCarrierPolicySelection = false;
        }

        UpdateCarrierPolicySummary(policy);
    }

    // A profile saved before the two modes were renamed reads back under the old names.
    private static string NormalizeCarrierPolicy(string policy) => policy switch
    {
        "relay_tcp_only" => "tcp",
        "quic_forced" => "quic",
        _ => policy,
    };

    private async void OnCarrierPolicySelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (suppressCarrierPolicySelection)
        {
            return;
        }

        if (SettingsCarrierPolicyComboBox.SelectedItem is not ComboBoxItem item ||
            item.Tag is not string policy)
        {
            return;
        }

        try
        {
            // The core moves the running connections at once — a migration or a close per peer, and a
            // mesh restart when the relay's TCP carrier was not bound — so not on the UI thread.
            await Task.Run(() => rustBridge.SetCarrierPolicy(policy));
            UpdateCarrierPolicySummary(policy);
        }
        catch (Exception ex)
        {
            SettingsCarrierPolicySummaryTextBlock.Text = $"Не удалось изменить несущую: {ex.Message}";
        }
    }

    private void UpdateCarrierPolicySummary(string policy)
    {
        // The core applies it to the running connections at once (`OmniApp::set_carrier_policy`).
        const string applies = " Применяется сразу.";
        // A connection the new mode refuses is closed at once (QA 2026-10-17: a call over UDP ended
        // when the contact had no TCP door to move to).
        const string breaks = " Связь, которую режим запрещает, рвётся сразу — идущий звонок может прерваться.";
        // The setting is about the first hop only: to the relay, or straight to the contact.
        SettingsCarrierPolicySummaryTextBlock.Text = policy switch
        {
            "tcp" or "relay_tcp_only" =>
                "Связь от вас до ближайшего узла — релея или самого контакта — только по TCP: до релея через " +
                "его TCP-вход, до контакта через его TCP-дверь. Работает там, где провайдер режет UDP; " +
                "медленнее. Дальше релей и контакт решают сами. С контактом без TCP-двери — только через релей." +
                applies + breaks,
            "quic" or "quic_forced" =>
                "Связь от вас до ближайшего узла — только QUIC по UDP. Быстрее всего, но если сеть режет " +
                "UDP, связь не поднимется — тогда выберите «Авто» или «TCP»." + applies + breaks,
            _ =>
                "Связь от вас до ближайшего узла — релея или самого контакта — по QUIC, где он проходит, " +
                "иначе по TCP; при обрыве переключается сама. Подходит большинству." + applies,
        };
    }
}
