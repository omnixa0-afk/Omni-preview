using System.Linq;
using System.Net;
using System.Net.NetworkInformation;
using System.Net.Sockets;
using System.Text.Json;
using System.Collections.Generic;
using System.Diagnostics;

namespace Omni.Windows.App.Core;

internal sealed partial class RustBridge
{
    private IReadOnlyList<NetworkInterfaceSnapshot>? cachedNetworkInventory;
    private bool isNetworkInventoryDirty = true;
    private bool networkInventoryEventsSubscribed;

    public TransportSnapshot GetTransportSnapshot()
        => CallJson(
            () => NativeMethods.omni_transport_snapshot(handle),
            OmniJsonSerializerContext.Default.TransportSnapshot,
            "transport_snapshot");

    public TransportDiscoveryRefreshPayload GetTransportDiscoveryRefreshPayload()
        => CallJson(
            () => NativeMethods.omni_transport_discovery_refresh(handle),
            OmniJsonSerializerContext.Default.TransportDiscoveryRefreshPayload,
            "transport_discovery_refresh");

    public IReadOnlyList<NetworkInterfaceSnapshot> GetLocalNetworkInventory()
        => GetCachedNetworkInventory();

    public ChatMessageRecord SendText(string peerSelector, string body)
        => CallJson(
            () => NativeMethods.omni_chat_send_text(
                handle,
                JsonSerializer.Serialize(
                    new SendTextRequest(peerSelector, body),
                    OmniJsonSerializerContext.Default.SendTextRequest)),
            OmniJsonSerializerContext.Default.ChatMessageRecord,
            "send_text");

    public ChatMessageRecord SendAttachment(
        string peerSelector,
        string body,
        string sourceFilePath,
        string? displayFileName,
        bool asPhoto)
        => CallJson(
            () => NativeMethods.omni_chat_send_attachment(
                handle,
                JsonSerializer.Serialize(
                    new SendAttachmentRequest(
                        peerSelector,
                        body,
                        sourceFilePath,
                        displayFileName,
                        // Always end-to-end: a file key per file. The «Только транспорт» choice is
                        // gone from the app (owner's «free protection only»; it made the user pick
                        // a cipher per file), and from the core since 2026-10-12.
                        "e2ee_media",
                        asPhoto),
                    OmniJsonSerializerContext.Default.SendAttachmentRequest)),
            OmniJsonSerializerContext.Default.ChatMessageRecord,
            "send_attachment");

    public ChatMessageRecord RequestAttachmentDownload(string messageId, string savePath)
        => CallJson(
            () => NativeMethods.omni_chat_request_attachment_download(
                handle,
                JsonSerializer.Serialize(
                    new AttachmentDownloadRequest(messageId, savePath),
                    OmniJsonSerializerContext.Default.AttachmentDownloadRequest)),
            OmniJsonSerializerContext.Default.ChatMessageRecord,
            "request_attachment_download");

    public ChatMessageRecord RevokeAttachmentAccess(string messageId)
        => CallJson(
            () => NativeMethods.omni_chat_revoke_attachment_access(
                handle,
                JsonSerializer.Serialize(
                    new MessageIdRequest(messageId),
                    OmniJsonSerializerContext.Default.MessageIdRequest)),
            OmniJsonSerializerContext.Default.ChatMessageRecord,
            "revoke_attachment_access");

    public ListMessagesResponse ListMessages(string peerKey)
        => CallJson(
            () => NativeMethods.omni_chat_list_messages(
                handle,
                JsonSerializer.Serialize(
                    new ListMessagesRequest(peerKey),
                    OmniJsonSerializerContext.Default.ListMessagesRequest)),
            OmniJsonSerializerContext.Default.ListMessagesResponse,
            "list_messages");

    public ListMessagesPageResponse ListMessagesPage(
        string dialogKey,
        long? beforeCreatedAtUnixMs,
        string? beforeMessageId,
        int limit)
        => CallJson(
            () => NativeMethods.omni_chat_list_messages_page(
                handle,
                JsonSerializer.Serialize(
                    new ListMessagesPageRequest(
                        dialogKey,
                        beforeCreatedAtUnixMs,
                        beforeMessageId,
                        limit),
                    OmniJsonSerializerContext.Default.ListMessagesPageRequest)),
            OmniJsonSerializerContext.Default.ListMessagesPageResponse,
            "list_messages_page");

    public OutboundQueueSnapshot GetQueueSnapshot()
        => CallJson(
            () => NativeMethods.omni_queue_snapshot(handle),
            OmniJsonSerializerContext.Default.OutboundQueueSnapshot,
            "queue_snapshot");

    public QueueDiagnosticsSnapshot GetQueueDiagnostics()
        => CallJson(
            () => NativeMethods.omni_queue_diagnostics(handle),
            OmniJsonSerializerContext.Default.QueueDiagnosticsSnapshot,
            "queue_diagnostics");

    private IReadOnlyList<NetworkInterfaceSnapshot> GetCachedNetworkInventory(bool forceRefresh = false)
    {
        lock (syncRoot)
        {
            if (forceRefresh || isNetworkInventoryDirty || cachedNetworkInventory is null)
            {
                cachedNetworkInventory = ReadCurrentNetworkInventory();
                isNetworkInventoryDirty = false;
            }

            return cachedNetworkInventory;
        }
    }

    private void SubscribeNetworkInventoryEvents()
    {
        lock (syncRoot)
        {
            if (networkInventoryEventsSubscribed)
            {
                return;
            }

            NetworkChange.NetworkAddressChanged += OnNetworkInventoryChanged;
            NetworkChange.NetworkAvailabilityChanged += OnNetworkAvailabilityChanged;
            networkInventoryEventsSubscribed = true;
        }
    }

    private void UnsubscribeNetworkInventoryEvents()
    {
        lock (syncRoot)
        {
            if (!networkInventoryEventsSubscribed)
            {
                return;
            }

            NetworkChange.NetworkAddressChanged -= OnNetworkInventoryChanged;
            NetworkChange.NetworkAvailabilityChanged -= OnNetworkAvailabilityChanged;
            networkInventoryEventsSubscribed = false;
        }
    }

    private void OnNetworkInventoryChanged(object? sender, EventArgs eventArgs)
        => InvalidateLocalNetworkInventory();

    private void OnNetworkAvailabilityChanged(object? sender, NetworkAvailabilityEventArgs eventArgs)
        => InvalidateLocalNetworkInventory();

    private void InvalidateLocalNetworkInventory()
    {
        lock (syncRoot)
        {
            isNetworkInventoryDirty = true;
        }
    }

    private static IReadOnlyList<NetworkInterfaceSnapshot> ReadCurrentNetworkInventory()
    {
        NetworkInterface[] networkInterfaces;
        try
        {
            networkInterfaces = NetworkInterface.GetAllNetworkInterfaces();
        }
        catch
        {
            return [];
        }

        return CollectSnapshotsSkippingFailures(
                networkInterfaces,
                BuildNetworkInterfaceSnapshot,
                DescribeNetworkInterface)
            .OrderByDescending(snapshot => snapshot.IsUp)
            .ThenBy(snapshot => snapshot.IsLoopback)
            .ThenBy(snapshot => snapshot.IsVirtual)
            .ThenBy(snapshot => snapshot.DisplayName, StringComparer.OrdinalIgnoreCase)
            .ToList();
    }

    internal static List<TSnapshot> CollectSnapshotsSkippingFailures<TInput, TSnapshot>(
        IEnumerable<TInput> inputs,
        Func<TInput, TSnapshot> projector,
        Func<TInput, string> describeInput)
    {
        List<TSnapshot> snapshots = [];

        foreach (TInput input in inputs)
        {
            try
            {
                snapshots.Add(projector(input));
            }
            catch (Exception ex)
            {
                string description;
                try
                {
                    description = describeInput(input);
                }
                catch
                {
                    description = typeof(TInput).Name;
                }

                if (string.IsNullOrWhiteSpace(description))
                {
                    description = typeof(TInput).Name;
                }

                Debug.WriteLine($"Skipping network inventory item '{description}': {ex}");
            }
        }

        return snapshots;
    }

    private static NetworkInterfaceSnapshot BuildNetworkInterfaceSnapshot(NetworkInterface networkInterface)
    {
        string id = !string.IsNullOrWhiteSpace(networkInterface.Id)
            ? networkInterface.Id
            : networkInterface.Name;
        string displayName = BuildNetworkInterfaceDisplayName(networkInterface);
        bool isLoopback = networkInterface.NetworkInterfaceType == NetworkInterfaceType.Loopback;
        bool isVirtual = IsVirtualNetworkInterface(networkInterface);
        bool isUp = networkInterface.OperationalStatus == OperationalStatus.Up;

        List<string> ipv4Addresses = [];
        List<(string Address, bool IsTemporary, bool IsPreferred)> ipv6Candidates = [];
        List<byte> prefixLengths = [];

        foreach (UnicastIPAddressInformation unicast in networkInterface.GetIPProperties().UnicastAddresses)
        {
            if (unicast.Address.AddressFamily == AddressFamily.InterNetworkV6)
            {
                string ipv6Text = unicast.Address.ToString().Trim();
                if (IPAddress.TryParse(ipv6Text, out IPAddress? ipv6Address) &&
                    ipv6Address.AddressFamily == AddressFamily.InterNetworkV6)
                {
                    ipv6Candidates.Add((
                        ipv6Text,
                        unicast.SuffixOrigin == SuffixOrigin.Random,
                        unicast.DuplicateAddressDetectionState == DuplicateAddressDetectionState.Preferred &&
                            unicast.AddressPreferredLifetime != 0));
                }

                continue;
            }

            if (unicast.Address.AddressFamily != AddressFamily.InterNetwork)
            {
                continue;
            }

            string addressText = unicast.Address.ToString().Trim();
            if (!IPAddress.TryParse(addressText, out IPAddress? ipAddress) ||
                ipAddress.AddressFamily != AddressFamily.InterNetwork)
            {
                continue;
            }

            ipv4Addresses.Add(addressText);
            prefixLengths.Add(GetPrefixLength(unicast.IPv4Mask));
        }

        bool supportsBroadcast = isUp && !isLoopback && !isVirtual && ipv4Addresses.Count > 0;
        List<string> ipv6Addresses = ipv6Candidates
            .OrderByDescending(candidate => candidate.IsTemporary)
            .ThenByDescending(candidate => candidate.IsPreferred)
            .Select(candidate => candidate.Address)
            .Distinct(StringComparer.OrdinalIgnoreCase)
            .ToList();

        return new NetworkInterfaceSnapshot(
            string.IsNullOrWhiteSpace(id) ? displayName : id,
            displayName,
            isUp,
            isVirtual,
            isLoopback,
            supportsBroadcast,
            networkInterface.SupportsMulticast,
            ipv4Addresses,
            prefixLengths,
            ipv6Addresses);
    }

    private static string DescribeNetworkInterface(NetworkInterface networkInterface)
    {
        string displayName = BuildNetworkInterfaceDisplayName(networkInterface);
        return string.IsNullOrWhiteSpace(displayName)
            ? networkInterface.Id
            : displayName;
    }

    private static string BuildNetworkInterfaceDisplayName(NetworkInterface networkInterface)
    {
        string name = networkInterface.Name?.Trim() ?? string.Empty;
        string description = networkInterface.Description?.Trim() ?? string.Empty;

        if (string.IsNullOrWhiteSpace(name))
        {
            return string.IsNullOrWhiteSpace(description) ? networkInterface.Id : description;
        }

        if (string.IsNullOrWhiteSpace(description) ||
            string.Equals(name, description, StringComparison.OrdinalIgnoreCase))
        {
            return name;
        }

        return $"{name} ({description})";
    }

    private static bool IsVirtualNetworkInterface(NetworkInterface networkInterface)
    {
        if (networkInterface.NetworkInterfaceType is NetworkInterfaceType.Loopback or NetworkInterfaceType.Tunnel)
        {
            return true;
        }

        string fingerprint = $"{networkInterface.Name} {networkInterface.Description}"
            .ToLowerInvariant();

        return fingerprint.Contains("virtual", StringComparison.Ordinal) ||
               fingerprint.Contains("vmware", StringComparison.Ordinal) ||
               fingerprint.Contains("hyper-v", StringComparison.Ordinal) ||
               fingerprint.Contains("virtualbox", StringComparison.Ordinal) ||
               fingerprint.Contains("vethernet", StringComparison.Ordinal) ||
               fingerprint.Contains("tunnel", StringComparison.Ordinal) ||
               fingerprint.Contains(" tun", StringComparison.Ordinal) ||
               fingerprint.Contains("-tun", StringComparison.Ordinal) ||
               fingerprint.Contains("tun ", StringComparison.Ordinal) ||
               fingerprint.Contains("tap-", StringComparison.Ordinal) ||
               fingerprint.Contains(" tap", StringComparison.Ordinal) ||
               fingerprint.Contains("vpn", StringComparison.Ordinal) ||
               fingerprint.Contains("wireguard", StringComparison.Ordinal) ||
               fingerprint.Contains("tailscale", StringComparison.Ordinal) ||
               fingerprint.Contains("zerotier", StringComparison.Ordinal) ||
               fingerprint.Contains("openvpn", StringComparison.Ordinal) ||
               fingerprint.Contains("sing-tun", StringComparison.Ordinal) ||
               fingerprint.Contains("docker", StringComparison.Ordinal) ||
               fingerprint.Contains("wsl", StringComparison.Ordinal) ||
               fingerprint.Contains("loopback", StringComparison.Ordinal) ||
               fingerprint.Contains("isatap", StringComparison.Ordinal) ||
               fingerprint.Contains("teredo", StringComparison.Ordinal);
    }

    private static byte GetPrefixLength(IPAddress? mask)
    {
        if (mask is null)
        {
            return 0;
        }

        byte[] bytes = mask.GetAddressBytes();
        int prefixLength = 0;
        bool seenZero = false;

        foreach (byte value in bytes)
        {
            for (int bit = 7; bit >= 0; bit--)
            {
                bool isSet = (value & (1 << bit)) != 0;
                if (isSet)
                {
                    if (seenZero)
                    {
                        return (byte)prefixLength;
                    }

                    prefixLength++;
                    continue;
                }

                seenZero = true;
            }
        }

        return (byte)Math.Clamp(prefixLength, 0, 32);
    }
}
