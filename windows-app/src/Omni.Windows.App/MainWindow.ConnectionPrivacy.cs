using System;
using Microsoft.UI.Xaml.Controls;

namespace Omni.Windows.App;

// Connection-privacy picker (Settings → "Сеть и обнаружение"): ONE understandable choice, not a
// row of implementation checkboxes — three points on "how much do I expose / lean on infra":
// Auto (connect automatically; full ladder incl. serverless rendezvous + direct P2P), Relay-only
// (rendezvous + relay are fine, but NEVER climb to a direct link — the data path stays relay-mediated
// so a contact never learns my IP), and Direct-only (LAN + router port-mapping + manual "код связи"
// exchange only; zero third parties). Self-contained partial: it resolves its own elements via the
// section accessor and drives the RustBridge connection-privacy FFI, so it touches no other MainWindow
// partial beyond a single Initialize call site. See docs/decisions/connection-privacy-modes.md +
// connection-quality-ladder.md (the Relay-only / direct-upgrade-off choice, tcp-direct follow-up d).
public sealed partial class MainWindow
{
    // Guards the SelectionChanged handler while we set SelectedIndex programmatically (mirrors
    // suppressListenerPortModeSelection) so populating the picker doesn't write the mode back.
    private bool suppressConnectionPrivacySelection;

    private ComboBox SettingsConnectionPrivacyComboBox =>
        ResolveSectionElement<ComboBox>(SettingsSectionHost, nameof(SettingsConnectionPrivacyComboBox));

    private TextBlock SettingsConnectionPrivacySummaryTextBlock =>
        ResolveSectionElement<TextBlock>(
            SettingsSectionHost,
            nameof(SettingsConnectionPrivacySummaryTextBlock));

    private void InitializeConnectionPrivacyPicker()
    {
        ComboBox combo = SettingsConnectionPrivacyComboBox;
        if (combo.Items.Count == 0)
        {
            combo.Items.Add(new ComboBoxItem
            {
                Content = "Напрямую, когда можно",
                Tag = "auto",
            });
            combo.Items.Add(new ComboBoxItem
            {
                Content = "Только через релей",
                Tag = "relay-only",
            });
            combo.Items.Add(new ComboBoxItem
            {
                Content = "Без посредников",
                Tag = "direct-only",
            });
            combo.SelectionChanged += OnConnectionPrivacySelectionChanged;
        }

        RefreshConnectionPrivacySelection();
    }

    private void RefreshConnectionPrivacySelection()
    {
        string mode;
        try
        {
            mode = rustBridge.GetConnectionPrivacyMode();
        }
        catch (Exception)
        {
            mode = "auto"; // best-effort: fall back to the default if the core can't be read yet
        }

        suppressConnectionPrivacySelection = true;
        try
        {
            ComboBox combo = SettingsConnectionPrivacyComboBox;
            int index = 0;
            for (int i = 0; i < combo.Items.Count; i++)
            {
                if (combo.Items[i] is ComboBoxItem item && item.Tag as string == mode)
                {
                    index = i;
                    break;
                }
            }

            combo.SelectedIndex = index;
        }
        finally
        {
            suppressConnectionPrivacySelection = false;
        }

        UpdateConnectionPrivacySummary(mode);
    }

    private void OnConnectionPrivacySelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (suppressConnectionPrivacySelection)
        {
            return;
        }

        if (SettingsConnectionPrivacyComboBox.SelectedItem is not ComboBoxItem item ||
            item.Tag is not string mode)
        {
            return;
        }

        try
        {
            rustBridge.SetConnectionPrivacyMode(mode);
            UpdateConnectionPrivacySummary(mode);
        }
        catch (Exception ex)
        {
            SettingsConnectionPrivacySummaryTextBlock.Text = $"Не удалось изменить режим: {ex.Message}";
        }
    }

    private void UpdateConnectionPrivacySummary(string mode)
    {
        SettingsConnectionPrivacySummaryTextBlock.Text = mode switch
        {
            "direct-only" =>
                "Ни почтовых ящиков, ни релеев: только локальная сеть и проброшенный порт. Если друг не нашёлся сам — " +
                "обменяйтесь кодом связи.",
            "relay-only" =>
                "Контакты не узнают ваш IP: связь всегда идёт через релей. Медленнее, и без релея не соединиться.",
            _ =>
                "Самое быстрое: Omni соединяет вас с контактом напрямую, а если не выходит — через релей. " +
                "Контакт при прямой связи видит ваш IP.",
        };
    }
}
