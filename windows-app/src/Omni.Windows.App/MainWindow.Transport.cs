using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Runtime.InteropServices.WindowsRuntime;
using System.Text.Json;
using System.Threading;
using Microsoft.UI.Text;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Automation;
using Microsoft.UI.Xaml.Controls;
using Windows.ApplicationModel.DataTransfer;
using Windows.Storage;
using Windows.Storage.FileProperties;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Chats;
using Omni.Windows.App.Features.Contacts;
using Omni.Windows.App.Presentation;
using Omni.Windows.App.ViewModels;

namespace Omni.Windows.App;

public sealed partial class MainWindow
{

    private void RefreshTransportStatus()
    {
        if (!rustBridge.IsInitialized || windowState.Onboarding.ShellMode != AppShellMode.Authenticated)
        {
            windowState.CurrentTransportPeerKey = null;
            windowState.CurrentPeerKey = null;
            windowState.SelectedContactId = null;
            selectedContactCard = null;
            lastQueueDiagnosticsSnapshot = null;
            TransportDiagnosticsTextBlock.Text = "Транспорт не инициализирован.";
            return;
        }

        TransportDiscoveryRefreshPayload refresh = rustBridge.GetTransportDiscoveryRefreshPayload();
        lastQueueDiagnosticsSnapshot = refresh.QueueDiagnosticsSnapshot;
        ApplyTransportRefreshPayload(new TransportRefreshPayload(refresh.TransportSnapshot, ReadMeshConnectionState()));
    }

    /// A section switch: the snapshot read OFF the UI thread (50–130 ms held the switch).
    private async void RefreshTransportStatusAsync()
    {
        if (!rustBridge.IsInitialized || windowState.Onboarding.ShellMode != AppShellMode.Authenticated)
        {
            RefreshTransportStatus();
            return;
        }

        try
        {
            await RefreshTransportDiscoveryQueueAsync();
        }
        catch (Exception ex)
        {
            AppendNavRefreshLog("async.transport_status", ex);
        }
    }

    private async Task RefreshTransportDiscoveryQueueAsync()
    {
        (TransportDiscoveryRefreshPayload refresh, MeshConnectionStateDto? mesh) = await Task.Run(
            () => (rustBridge.GetTransportDiscoveryRefreshPayload(), ReadMeshConnectionState()));
        lastQueueDiagnosticsSnapshot = refresh.QueueDiagnosticsSnapshot;
        ApplyTransportRefreshPayload(new TransportRefreshPayload(refresh.TransportSnapshot, mesh));
        ApplyQueueDiagnosticsRefreshPayload(new QueueDiagnosticsRefreshPayload(
            refresh.QueueDiagnosticsSnapshot,
            GetResolvedCurrentDialogKey()));
    }

    private void ApplyTransportRefreshPayload(TransportRefreshPayload payload)
    {
        lastTransportSnapshot = payload.Snapshot;
        cachedConfiguredTransportMode = TransportModeExtensions.FromDbValue(payload.Snapshot.TransportModeValue);
        BindCurrentPeerFromTransport(payload.Snapshot);
        TransportDiagnosticsTextBlock.Text = MainWindowPresentation.BuildTransportDiagnostics(lastTransportSnapshot, payload.Mesh);
        if (windowState.CurrentSection == AuthenticatedShellSection.Diagnostics)
        {
            RenderHelpStatus(payload.Mesh);
        }
    }

    // Read with the snapshot, off the UI thread: on the UI thread it held the window 40-120 ms on every
    // transport tick while Contacts, Settings or Diagnostics were open.
    private MeshConnectionStateDto? ReadMeshConnectionState()
    {
        try
        {
            MeshConnectionStateDto state = rustBridge.MeshConnectionState();
            lastMeshReading = new MeshReading(state, System.Diagnostics.Stopwatch.GetTimestamp());
            return state;
        }
        catch
        {
            // Mesh not started for this profile: the card says so from the snapshot alone.
            return null;
        }
    }

    private sealed record MeshReading(MeshConnectionStateDto State, long At);

    private static readonly TimeSpan MeshReadingFreshFor = TimeSpan.FromMilliseconds(500);
    private volatile MeshReading? lastMeshReading;

    // The mesh state for drawing on the UI thread: one read costs 40-120 ms, and a single refresh of a
    // screen asked for it several times. An off-thread read made just before applying (read AFTER the
    // data it goes with, so what is drawn is never older than what decided to draw it) serves them all;
    // without one this reads here, as before.
    private MeshConnectionStateDto MeshStateForScreen()
    {
        if (lastMeshReading is { } reading &&
            System.Diagnostics.Stopwatch.GetElapsedTime(reading.At) < MeshReadingFreshFor)
        {
            return reading.State;
        }

        MeshConnectionStateDto state = rustBridge.MeshConnectionState();
        lastMeshReading = new MeshReading(state, System.Diagnostics.Stopwatch.GetTimestamp());
        return state;
    }

    // Honest node-status line, derived from the mesh connection-state DTO (Rust = truth): whether
    // the mesh is up and whether contacts can reach this node directly.
    private string BuildMeshNodeStatusText()
    {
        string text;
        try
        {
            var node = MeshStateForScreen().Node;
            text = node.Kind switch
            {
                "online_reachable" => node.Endpoints is { Count: > 0 }
                    ? $"В сети · контакты дозвонятся напрямую: {string.Join(", ", node.Endpoints)}"
                    : "В сети · контакты дозвонятся напрямую",
                "online_behind_nat" => "В сети · напрямую к вам не попасть — контакты дойдут через других",
                "connecting" => "Подключаюсь к сети…",
                "isolated" => "Изолированы · нет ни локальной сети, ни внешнего адреса",
                "not_started" => "Сеть не запущена",
                _ => "Состояние сети неизвестно",
            };
        }
        catch (Exception)
        {
            // Mesh not started yet / transient — keep an honest neutral line.
            text = "Состояние сети недоступно";
        }

        return text;
    }

    private void BindCurrentPeerFromTransport(TransportSnapshot snapshot)
    {
        if (snapshot.State is not ("connected" or "listening_and_connected"))
        {
            windowState.CurrentTransportPeerKey = null;
            return;
        }

        string? resolvedDialogKey = ResolveDialogKeyFromTransportSnapshot(snapshot);
        if (!string.IsNullOrWhiteSpace(resolvedDialogKey))
        {
            windowState.CurrentTransportPeerKey = resolvedDialogKey;
        }
        else
        {
            windowState.CurrentTransportPeerKey = null;
        }

        SaveUiState();
        UpdateCurrentPeerText();
    }

    private void UpdateCurrentPeerText()
    {
        // This third header line shows the dialog's connection status, not the peer name:
        // the name is already the header title (ChatIdentityTitleTextBlock), so the old
        // "Чат с {peer}" duplicated the title and the empty-state "Выберите чат слева…"
        // duplicated the subtitle prompt (three names / three prompts at once — see
        // docs/bugs/chats-history-collapse-stacked-layout.md). BuildPeerSubtitle returns a
        // neutral status instead ("Диалог пока не выбран") — и пустую строку, когда диалог выбран:
        // пустой TextBlock надо СВОРАЧИВАТЬ, иначе в шапке остаётся висеть пустая строка вместо
        // снятого утверждения.
        string peerSubtitle = MainWindowPresentation.BuildPeerSubtitle(GetResolvedCurrentDialogKey());
        CurrentPeerTextBlock.Text = peerSubtitle;
        CurrentPeerTextBlock.Visibility = string.IsNullOrEmpty(peerSubtitle)
            ? Visibility.Collapsed
            : Visibility.Visible;
    }


}
