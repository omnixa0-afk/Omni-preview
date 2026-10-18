using System;
using System.Globalization;
using Microsoft.UI.Xaml.Controls;

namespace Omni.Windows.App;

// Fixed direct-TCP front-door port (Settings → «Дополнительно»): the reliable relay-less inbound
// path. The ephemeral UPnP-mapped port proved unreliable live (a mobile peer's TCP reconnect to a
// phantom-forwarded map failed). When the user pins a port here AND port-forwards it on their router,
// a peer on a UDP-hostile network (mobile) reaches us relay-less over outbound TCP that survives UDP
// drops. "0"/blank = ephemeral (rely on UPnP). Drives the RustBridge direct-tcp-port FFI; applies on
// the next mesh start. See docs/decisions/tcp-direct-peer-fallback.md.
public sealed partial class MainWindow
{
    private TextBox SettingsDirectTcpPortTextBox =>
        ResolveSectionElement<TextBox>(SettingsSectionHost, nameof(SettingsDirectTcpPortTextBox));

    private TextBlock SettingsDirectTcpPortSummaryTextBlock =>
        ResolveSectionElement<TextBlock>(SettingsSectionHost, nameof(SettingsDirectTcpPortSummaryTextBlock));

    private void InitializeDirectTcpPortField()
    {
        TextBox box = SettingsDirectTcpPortTextBox;
        // Wire commit handlers exactly once.
        box.LostFocus -= OnDirectTcpPortCommitted;
        box.LostFocus += OnDirectTcpPortCommitted;
        box.KeyDown -= OnDirectTcpPortKeyDown;
        box.KeyDown += OnDirectTcpPortKeyDown;

        RefreshDirectTcpPortField();
    }

    private void RefreshDirectTcpPortField()
    {
        string port;
        try
        {
            port = rustBridge.GetDirectTcpPort();
        }
        catch (Exception)
        {
            port = "0"; // best-effort: fall back to ephemeral if the core can't be read yet
        }

        SettingsDirectTcpPortTextBox.Text = (port.Trim() == "0") ? string.Empty : port.Trim();
        UpdateDirectTcpPortSummary(port.Trim());
    }

    private void OnDirectTcpPortKeyDown(object sender, Microsoft.UI.Xaml.Input.KeyRoutedEventArgs e)
    {
        if (e.Key == global::Windows.System.VirtualKey.Enter)
        {
            e.Handled = true;
            CommitDirectTcpPort();
        }
    }

    private void OnDirectTcpPortCommitted(object sender, Microsoft.UI.Xaml.RoutedEventArgs e)
        => CommitDirectTcpPort();

    private void CommitDirectTcpPort()
    {
        string raw = SettingsDirectTcpPortTextBox.Text?.Trim() ?? string.Empty;
        // Blank means "clear back to ephemeral" — send "0".
        string toSend = raw.Length == 0 ? "0" : raw;

        // Validate locally so a typo surfaces a friendly message instead of a core error envelope.
        if (raw.Length != 0 &&
            (!int.TryParse(raw, NumberStyles.None, CultureInfo.InvariantCulture, out int parsed)
             || parsed < 1 || parsed > 65535))
        {
            SettingsDirectTcpPortSummaryTextBlock.Text =
                "Введите порт от 1 до 65535 (или оставьте пустым для авто/UPnP).";
            return;
        }

        try
        {
            rustBridge.SetDirectTcpPort(toSend);
            UpdateDirectTcpPortSummary(toSend);
        }
        catch (Exception ex)
        {
            SettingsDirectTcpPortSummaryTextBlock.Text = $"Не удалось задать порт: {ex.Message}";
        }
    }

    private void UpdateDirectTcpPortSummary(string port)
    {
        const string applies = " Применится при следующем подключении.";
        SettingsDirectTcpPortSummaryTextBlock.Text = (string.IsNullOrEmpty(port) || port == "0")
            ? "Порт TCP-двери выбирается автоматически и пробрасывается через UPnP. Надёжно не на всех " +
              "роутерах." + applies
            : $"TCP-дверь слушает порт {port}. Пробросьте этот TCP-порт на роутере на этот ПК " +
              "(Port Forwarding → TCP), чтобы собеседник с мобильного гарантированно дозвонился без релея." +
              applies;
    }
}
