using Omni.Windows.App.Core;
using System.Net;

namespace Omni.Windows.App.Presentation;

internal static partial class MainWindowPresentation
{
    public static string BuildConnectionStatus(TransportSnapshot? snapshot)
    {
        if (snapshot is null)
        {
            return "Ожидание подключения";
        }

        if (snapshot.State == "connected")
        {
            return string.IsNullOrWhiteSpace(snapshot.PeerId)
                ? "Подключено"
                : $"Подключено: {snapshot.PeerId}";
        }

        if (!string.IsNullOrWhiteSpace(snapshot.LastError))
        {
            return $"Не удалось подключиться: {snapshot.LastError}";
        }

        return "Ожидание подключения";
    }

    public static string BuildPeerSubtitle(string? peerKey)
    {
        if (string.IsNullOrWhiteSpace(peerKey))
        {
            return "Диалог пока не выбран";
        }

        // Раньше здесь стояло «Активный чат подключён» — при том, что функция не знает о связи
        // РОВНО НИЧЕГО: она видит только выбранный диалог и сохранённый адрес. Строка писала
        // «подключён» рядом с честным «Офлайн» и значком «Доставлю, когда появится», то есть
        // ровно то противоречие, ради которого написан
        // docs/decisions/reachability-belongs-to-the-session.md. Достижимость показывает шапка по
        // ContactConnectionDto; здесь молчим. (Строка «Активный чат: {адрес}» ушла вместе с адресом
        // набора старого движка, 2026-10-14.)
        return string.Empty;
    }

    /// <summary>
    /// The «Транспорт» card in Diagnostics: this node on the mesh, how many contacts are linked and
    /// how, the active session and the last transport error. It used to list the listener of the
    /// old QUIC engine (bind, local / public endpoint, port, dial address) — removed with that
    /// engine on 2026-10-04, so every one of those lines said «не задан» forever.
    /// </summary>
    public static string BuildTransportDiagnostics(TransportSnapshot? snapshot, MeshConnectionStateDto? mesh)
    {
        if (snapshot is null && mesh is null)
        {
            return "Транспорт не инициализирован.";
        }

        List<string> lines = [];
        if (mesh is not null)
        {
            lines.Add($"Узел в сети: {FormatNodeKind(mesh.Node.Kind)}");
            if (mesh.Node.Endpoints is { Count: > 0 } endpoints)
            {
                lines.Add($"Внешние адреса: {string.Join(", ", endpoints)}");
            }

            int direct = mesh.Contacts.Count(contact => contact.Link == "online_direct");
            int relayed = mesh.Contacts.Count(contact => contact.Link == "online_via_relay");
            int searching = mesh.Contacts.Count(contact => contact.Link == "searching");
            lines.Add($"Контакты на связи: напрямую {direct}, через сеть {relayed}, ищем {searching} (из {mesh.Contacts.Count})");
            var carriers = mesh.Contacts
                .Where(contact => !string.IsNullOrEmpty(contact.Carrier))
                .GroupBy(contact => contact.Carrier!)
                .Select(group => $"{FormatCarrierName(group.Key)} {group.Count()}")
                .ToList();
            if (carriers.Count > 0)
            {
                lines.Add($"Несущие: {string.Join(", ", carriers)}");
            }
        }

        if (snapshot is not null)
        {
            lines.Add(snapshot.State == "connected"
                ? $"Активная сессия: {snapshot.RemoteDeviceName ?? snapshot.PeerKey ?? "есть"}{(string.IsNullOrWhiteSpace(snapshot.PeerId) ? string.Empty : $" ({snapshot.PeerId})")}"
                : "Активная сессия: нет");
            if (!string.IsNullOrWhiteSpace(snapshot.LastError))
            {
                lines.Add($"Последняя ошибка: {snapshot.LastError}");
            }
        }

        return string.Join("\n", lines);
    }

    private static string FormatNodeKind(string kind) => kind switch
    {
        "not_started" => "не запущен",
        "isolated" => "без сети (только локальная)",
        "connecting" => "подключается",
        "online_behind_nat" => "в сети, за NAT",
        "online_reachable" => "в сети, достижим напрямую",
        _ => kind,
    };

    private static string FormatCarrierName(string carrier) => carrier switch
    {
        "quic" => "QUIC",
        "tcp_door" => "TCP-дверь",
        "tcp_fallback" => "TCP-фолбэк",
        "lan" => "LAN",
        _ => carrier,
    };

    public static string BuildNetworkInterfaceLabel(NetworkInterfaceSnapshot snapshot)
    {
        List<string> parts = [snapshot.DisplayName];

        List<string> addresses = snapshot.IPv4Addresses
            .Zip(snapshot.PrefixLengths, (address, prefixLength) => $"{address}/{prefixLength}")
            .Where(address => !string.IsNullOrWhiteSpace(address))
            .Take(2)
            .ToList();

        if (addresses.Count > 0)
        {
            parts.Add($"({string.Join(", ", addresses)})");
        }

        if (!snapshot.IsUp)
        {
            parts.Add("• недоступен");
        }

        return string.Join(" ", parts);
    }

    private static string FormatAge(long ageMs)
    {
        if (ageMs < 1_000)
        {
            return "только что";
        }

        long seconds = ageMs / 1_000;
        if (seconds < 60)
        {
            return $"{seconds} сек назад";
        }

        long minutes = seconds / 60;
        if (minutes < 60)
        {
            return $"{minutes} мин назад";
        }

        long hours = minutes / 60;
        return $"{hours} ч назад";
    }

    private static string FormatDuration(TimeSpan duration)
    {
        if (duration.TotalHours >= 1d)
        {
            return $"{(int)duration.TotalHours:00}:{duration.Minutes:00}:{duration.Seconds:00}";
        }

        return $"{duration.Minutes:00}:{duration.Seconds:00}";
    }

    private static string? TryExtractEndpointHost(string endpoint)
    {
        string trimmed = endpoint.Trim();
        if (trimmed.StartsWith("/ip4/", StringComparison.OrdinalIgnoreCase) ||
            trimmed.StartsWith("/ip6/", StringComparison.OrdinalIgnoreCase))
        {
            string[] parts = trimmed.Split('/', StringSplitOptions.RemoveEmptyEntries);
            if (parts.Length >= 2 &&
                (string.Equals(parts[0], "ip4", StringComparison.OrdinalIgnoreCase) ||
                 string.Equals(parts[0], "ip6", StringComparison.OrdinalIgnoreCase)))
            {
                return parts[1];
            }
        }

        if (trimmed.StartsWith('['))
        {
            int bracketEnd = trimmed.IndexOf(']');
            if (bracketEnd > 1 &&
                bracketEnd < trimmed.Length - 1 &&
                trimmed[bracketEnd + 1] == ':')
            {
                return trimmed[1..bracketEnd];
            }
        }

        int separatorIndex = trimmed.LastIndexOf(':');
        if (separatorIndex <= 0 || separatorIndex >= trimmed.Length - 1)
        {
            return null;
        }

        return trimmed[..separatorIndex];
    }

    private static NetworkInterfaceSnapshot? TryFindInventoryMatch(
        string? localHost,
        string? peerHost,
        IReadOnlyList<NetworkInterfaceSnapshot> inventory)
    {
        if (!string.IsNullOrWhiteSpace(localHost))
        {
            NetworkInterfaceSnapshot? exactLocalMatch = inventory.FirstOrDefault(network =>
                NetworkHasAddress(network, localHost));
            if (exactLocalMatch is not null)
            {
                return exactLocalMatch;
            }
        }

        if (!string.IsNullOrWhiteSpace(peerHost))
        {
            NetworkInterfaceSnapshot? exactPeerMatch = inventory.FirstOrDefault(network =>
                NetworkHasAddress(network, peerHost));
            if (exactPeerMatch is not null)
            {
                return exactPeerMatch;
            }

            if (IPAddress.TryParse(peerHost.Trim('[', ']'), out IPAddress? peerIp))
            {
                if (peerIp.AddressFamily == System.Net.Sockets.AddressFamily.InterNetwork)
                {
                    foreach (NetworkInterfaceSnapshot network in inventory)
                    {
                        for (int i = 0; i < Math.Min(network.IPv4Addresses.Count, network.PrefixLengths.Count); i++)
                        {
                            if (!IPAddress.TryParse(network.IPv4Addresses[i], out IPAddress? localIp) ||
                                localIp.AddressFamily != System.Net.Sockets.AddressFamily.InterNetwork)
                            {
                                continue;
                            }

                            if (IsSameIpv4Subnet(localIp, peerIp, network.PrefixLengths[i]))
                            {
                                return network;
                            }
                        }
                    }
                }
                else if (peerIp.AddressFamily == System.Net.Sockets.AddressFamily.InterNetworkV6)
                {
                    foreach (NetworkInterfaceSnapshot network in inventory)
                    {
                        foreach (string address in network.IPv6Addresses ?? [])
                        {
                            if (!IPAddress.TryParse(address, out IPAddress? localIp) ||
                                localIp.AddressFamily != System.Net.Sockets.AddressFamily.InterNetworkV6)
                            {
                                continue;
                            }

                            if (IsSameIpv6Subnet(localIp, peerIp, 64))
                            {
                                return network;
                            }
                        }
                    }
                }
            }
        }

        return null;
    }

    private static bool NetworkHasAddress(NetworkInterfaceSnapshot network, string host)
        => network.IPv4Addresses.Any(address => string.Equals(address, host, StringComparison.OrdinalIgnoreCase)) ||
           (network.IPv6Addresses?.Any(address => string.Equals(address, host.Trim('[', ']'), StringComparison.OrdinalIgnoreCase)) ?? false);

    private static string? TryBuildLoopbackNetworkLabel(string? localHost, string? peerHost)
    {
        foreach (string? host in new[] { localHost, peerHost })
        {
            if (string.IsNullOrWhiteSpace(host))
            {
                continue;
            }

            if (IPAddress.TryParse(host, out IPAddress? ipAddress) && IPAddress.IsLoopback(ipAddress))
            {
                return $"Loopback ({host})";
            }
        }

        return null;
    }

    private static bool IsPublicIpv4InternetHost(string? host)
    {
        if (string.IsNullOrWhiteSpace(host) ||
            !IPAddress.TryParse(host.Trim('[', ']'), out IPAddress? ipAddress) ||
            ipAddress.AddressFamily != System.Net.Sockets.AddressFamily.InterNetwork)
        {
            return false;
        }

        byte[] bytes = ipAddress.GetAddressBytes();
        return !IPAddress.IsLoopback(ipAddress) &&
               !ipAddress.Equals(IPAddress.Any) &&
               !ipAddress.Equals(IPAddress.None) &&
               bytes[0] != 10 &&
               bytes[0] != 127 &&
               bytes[0] < 224 &&
               !(bytes[0] == 169 && bytes[1] == 254) &&
               !(bytes[0] == 172 && bytes[1] is >= 16 and <= 31) &&
               !(bytes[0] == 192 && bytes[1] == 168) &&
               !(bytes[0] == 100 && bytes[1] is >= 64 and <= 127);
    }

    private static bool IsIpv6Host(string? host)
        => !string.IsNullOrWhiteSpace(host) &&
           IPAddress.TryParse(host.Trim('[', ']'), out IPAddress? ipAddress) &&
           ipAddress.AddressFamily == System.Net.Sockets.AddressFamily.InterNetworkV6;

    private static bool IsSameIpv4Subnet(IPAddress localIp, IPAddress peerIp, int prefixLength)
    {
        byte[] localBytes = localIp.GetAddressBytes();
        byte[] peerBytes = peerIp.GetAddressBytes();
        if (localBytes.Length != 4 || peerBytes.Length != 4)
        {
            return false;
        }

        int normalizedPrefix = Math.Clamp(prefixLength, 0, 32);
        int fullBytes = normalizedPrefix / 8;
        int remainingBits = normalizedPrefix % 8;

        for (int i = 0; i < fullBytes; i++)
        {
            if (localBytes[i] != peerBytes[i])
            {
                return false;
            }
        }

        if (remainingBits == 0)
        {
            return true;
        }

        int mask = 0xFF << (8 - remainingBits);
        return (localBytes[fullBytes] & mask) == (peerBytes[fullBytes] & mask);
    }

    private static bool IsSameIpv6Subnet(IPAddress localIp, IPAddress peerIp, int prefixLength)
    {
        byte[] localBytes = localIp.GetAddressBytes();
        byte[] peerBytes = peerIp.GetAddressBytes();
        if (localBytes.Length != 16 || peerBytes.Length != 16)
        {
            return false;
        }

        int normalizedPrefix = Math.Clamp(prefixLength, 0, 128);
        int fullBytes = normalizedPrefix / 8;
        int remainingBits = normalizedPrefix % 8;

        for (int i = 0; i < fullBytes; i++)
        {
            if (localBytes[i] != peerBytes[i])
            {
                return false;
            }
        }

        if (remainingBits == 0)
        {
            return true;
        }

        int mask = 0xFF << (8 - remainingBits);
        return (localBytes[fullBytes] & mask) == (peerBytes[fullBytes] & mask);
    }

    private static string NormalizeEndpointForDisplay(string endpoint)
    {
        string trimmed = endpoint.Trim();
        if (trimmed.StartsWith("/ip4/", StringComparison.OrdinalIgnoreCase) ||
            trimmed.StartsWith("/ip6/", StringComparison.OrdinalIgnoreCase))
        {
            string[] parts = trimmed.Split('/', StringSplitOptions.RemoveEmptyEntries);
            if (parts.Length >= 4 &&
                (string.Equals(parts[0], "ip4", StringComparison.OrdinalIgnoreCase) ||
                 string.Equals(parts[0], "ip6", StringComparison.OrdinalIgnoreCase)) &&
                (string.Equals(parts[2], "tcp", StringComparison.OrdinalIgnoreCase) ||
                 string.Equals(parts[2], "udp", StringComparison.OrdinalIgnoreCase)))
            {
                return $"{parts[1]}:{parts[3]}";
            }
        }

        return trimmed;
    }

    private static string MapTransportState(string state)
        => state switch
        {
            "idle" => "ожидание",
            "listening" => "слушает входящие подключения",
            "connected" => "подключено",
            "listening_and_connected" => "слушает и подключено",
            _ => state,
        };

    private static string FormatFailure(string? kind, string? message)
    {
        if (string.IsNullOrWhiteSpace(kind) && string.IsNullOrWhiteSpace(message))
        {
            return "нет";
        }

        if (string.IsNullOrWhiteSpace(kind))
        {
            return message ?? "нет";
        }

        if (string.IsNullOrWhiteSpace(message))
        {
            return kind;
        }

        return $"{kind}: {message}";
    }

    private static string MapSecurityWarning(string? warning)
        => (warning ?? string.Empty).ToLowerInvariant() switch
        {
            "none" => "Нет активных предупреждений",
            "unlinkeddialog" or "unlinked_dialog" => "Диалог ещё не привязан к контакту",
            "missingsession" or "missing_session" => "Защищённая сессия ещё не установлена",
            "missingremotebundle" or "missing_remote_bundle" => "Идёт автоматический обмен ключами",
            "unverified" => "Контакт ещё не подтверждён вручную",
            "reverifyneeded" or "reverify_needed" => "Нужно заново проверить контакт",
            "identitychanged" or "identity_changed" => "Обнаружено изменение идентичности",
            "blockedcontact" or "blocked_contact" => "Контакт отклонён",
            _ => "Нет активных предупреждений",
        };

    private static string MapSessionState(string? state)
        => (state ?? string.Empty).ToLowerInvariant() switch
        {
            "initiatorbootstrapcreated" or "initiator_bootstrap_created" => "bootstrap создан, ждём принятия",
            "responderbootstrapaccepted" or "responder_bootstrap_accepted" => "bootstrap принят удалённой стороной",
            "established" => "сессия установлена",
            _ => "нет активной сессии",
        };

    private static string MapPayloadKind(string? payloadKind)
        => (payloadKind ?? string.Empty).ToLowerInvariant() switch
        {
            "sessionbootstrap" or "session_bootstrap" => "bootstrap сессии",
            "text" => "зашифрованный текст",
            "deliveryreceipt" or "delivery_receipt" => "зашифрованное подтверждение доставки",
            _ => "неизвестно",
        };

    private static string MapEncryptionState(string state)
        => state.ToLowerInvariant() switch
        {
            "legacyplaintext" or "legacy_plaintext" => "старый незашифрованный формат",
            "bootstrapsent" or "bootstrap_sent" => "bootstrap отправлен",
            "encryptedpending" or "encrypted_pending" => "ожидает защищённой доставки",
            "encrypteddelivered" or "encrypted_delivered" => "зашифрованная доставка подтверждена",
            "encryptedreceived" or "encrypted_received" => "получено в зашифрованном виде",
            _ => "неизвестно",
        };

    private static string FormatOptional(string? value)
        => string.IsNullOrWhiteSpace(value) ? "нет" : value;

    private static string FormatTransportMode(string? transportModeValue)
    {
        return transportModeValue?.Trim().ToLowerInvariant() switch
        {
            "quic" => "QUIC",
            null or "" => "не задан",
            _ => transportModeValue,
        };
    }

}


