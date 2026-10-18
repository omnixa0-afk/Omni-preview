using System;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;

namespace Omni.Windows.App;

// Mesh pairing UI handlers (om3mesh: invite generate/import). This is the mesh
// replacement for the legacy om3: invite path — see
// docs/decisions/pairing-over-mesh-adoption.md and CONTRIBUTING.md §Development stage.
// The legacy invite UI + its handlers in the frozen MainWindow.Signaling.cs are
// superseded and slated for removal once mesh pairing is the default (don't patch
// that frozen file; remove it in the dedicated legacy-signaling-removal pass).
//
// Both bridge calls are cheap (generate = encode; register = insert a pairing
// target), so running them on the UI thread is fine — no Task.Run needed.
public sealed partial class MainWindow
{
    private void OnGenerateMeshInviteClicked(object sender, RoutedEventArgs e)
    {
        try
        {
            GeneratedMeshInviteTextBox.Text = rustBridge.GenerateMeshPairingInvite();
            // Also show whether friends can reach us directly (UPnP/IPv6) or over the mesh —
            // best-effort, a cheap read of mesh state (no network), so a failure just omits it.
            // Derived in the shell from the honest connection-state DTO (Rust = truth, C# = render).
            string reachability = string.Empty;
            try
            {
                reachability = FormatNodeReachability(rustBridge.MeshConnectionState().Node);
            }
            catch (Exception)
            {
                // Mesh not started yet / transient — leave the reachability line off.
            }

            MeshPairingStatusTextBlock.Text = reachability.Length > 0
                ? $"Приглашение готово — отправьте его другу.\n{reachability}"
                : "Приглашение готово — отправьте его другу.";
        }
        catch (Exception ex)
        {
            MeshPairingStatusTextBlock.Text = $"Не удалось создать приглашение: {ex.Message}";
        }
    }

    // Format this node's connectivity (from the honest connection-state DTO) into a short line for
    // the pairing UI. Presentation lives in the shell; the core derives the state (Rust = truth).
    private static string FormatNodeReachability(NodeConnectivityDto node)
        => node.Kind switch
        {
            "online_reachable" => node.Endpoints is { Count: > 0 }
                ? $"Вы в сети и достижимы напрямую: {string.Join(", ", node.Endpoints)} — друзья дозвонятся без релея."
                : "Вы в сети и достижимы напрямую — друзья дозвонятся без релея.",
            "online_behind_nat" => "Вы в сети. Напрямую к вам не попасть, но друзья найдут вас через других.",
            "connecting" => "Подключаюсь к сети…",
            "isolated" => "Похоже, вы изолированы — нет ни локальной сети, ни внешнего адреса.",
            _ => string.Empty, // not_started / unknown → no extra line
        };

    private void OnPairOverMeshClicked(object sender, RoutedEventArgs e)
    {
        string token = ImportMeshInviteTextBox.Text?.Trim() ?? string.Empty;
        if (token.Length == 0)
        {
            MeshPairingStatusTextBlock.Text = "Вставьте приглашение друга.";
            return;
        }

        try
        {
            rustBridge.RegisterMeshPairingInvite(token);
            MeshPairingStatusTextBlock.Text =
                "Приглашение принято. Связь установится сама — подтвердите контакт, когда он появится.";
        }
        catch (Exception ex)
        {
            MeshPairingStatusTextBlock.Text = $"Не удалось импортировать приглашение: {ex.Message}";
        }
    }

    // "Код связи" (reach token) — the manual, no-third-party reach to an already-paired friend (and
    // the primary path in Direct-only). Export carries our current endpoints; import dials them.
    // Both bridge calls are cheap (export = gather endpoints + encode; import = dial_at), so the UI
    // thread is fine. See docs/decisions/connection-privacy-modes.md.
    private void OnGenerateReachTokenClicked(object sender, RoutedEventArgs e)
    {
        try
        {
            GeneratedReachTokenTextBox.Text = rustBridge.GenerateReachToken();
            ReachTokenStatusTextBlock.Text =
                "Код связи готов — отправьте его другу любым способом, которому доверяете. Действует, пока не сменится сеть.";
        }
        catch (Exception ex)
        {
            ReachTokenStatusTextBlock.Text = $"Не удалось получить код связи: {ex.Message}";
        }
    }

    private void OnDialReachTokenClicked(object sender, RoutedEventArgs e)
    {
        string token = ImportReachTokenTextBox.Text?.Trim() ?? string.Empty;
        if (token.Length == 0)
        {
            ReachTokenStatusTextBlock.Text = "Вставьте om3reach:-код друга.";
            return;
        }

        try
        {
            rustBridge.DialReachToken(token);
            ReachTokenStatusTextBlock.Text = "Подключаюсь по коду связи…";
        }
        catch (Exception ex)
        {
            ReachTokenStatusTextBlock.Text = $"Не удалось подключиться по коду: {ex.Message}";
        }
    }

    private void OnCopyMeshInviteClicked(object sender, RoutedEventArgs e)
        => CopyTokenToClipboard(GeneratedMeshInviteTextBox.Text, MeshPairingStatusTextBlock, "Приглашение скопировано в буфер обмена.");

    private void OnCopyReachTokenClicked(object sender, RoutedEventArgs e)
        => CopyTokenToClipboard(GeneratedReachTokenTextBox.Text, ReachTokenStatusTextBlock, "Код связи скопирован в буфер обмена.");

    // Copy a generated token to the clipboard one-tap (so the user doesn't hand-select the box).
    private static void CopyTokenToClipboard(string text, TextBlock status, string okMessage)
    {
        if (string.IsNullOrWhiteSpace(text))
        {
            status.Text = "Сначала сгенерируйте значение.";
            return;
        }

        var package = new global::Windows.ApplicationModel.DataTransfer.DataPackage();
        package.SetText(text);
        global::Windows.ApplicationModel.DataTransfer.Clipboard.SetContent(package);
        status.Text = okMessage;
    }
}
