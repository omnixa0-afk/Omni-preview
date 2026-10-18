using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Chats;
using Omni.Windows.App.Presentation;
using Omni.Windows.App.ViewModels;
using Windows.Foundation;

namespace Omni.Windows.App;

// Chat header summary: builds the per-dialog header (identity, presence dot, trust/session
// state) from the header refresh payload, plus end-session / selection-fallback helpers.
// Partial of MainWindow split from MainWindow.Chats.cs; same class, identical IL, no behaviour change.
public sealed partial class MainWindow
{
    private void RefreshChatHeaderSummary()
    {
        string? dialogKey = GetResolvedCurrentDialogKey();
        if (string.IsNullOrWhiteSpace(dialogKey))
        {
            ClearChatHeaderSummaryState();
            return;
        }

        try
        {
            ChatHeaderRefreshPayload? payload = chatRefreshService.FetchHeader(
                rustBridge,
                dialogKey,
                CancellationToken.None);
            if (payload is null)
            {
                return;
            }

            ApplyChatHeaderRefreshPayload(payload);
        }
        catch (Exception ex)
        {
            ApplyChatHeaderRefreshFailure(ex);
        }
    }

    private void ClearChatHeaderSummaryState()
    {
        WithSelectionSyncSuppressed(ref suppressChatSelectionSync, () =>
        {
            ChatsListView.SelectedItem = null;
        });

        currentChatHeaderSummary = null;
        currentChatSecuritySummary = null;
        currentChatSessionSummary = null;
        currentChatCallSummary = null;
        ApplyAvatarVisual(
            ChatIdentityAvatarImage,
            ChatIdentityAvatarTextBlock,
            null,
            "?",
            loadingRing: ChatIdentityAvatarLoadingRing,
            isLoading: false,
            versionToken: null);
        ApplyPresenceDot(ChatHeaderPresenceDotBorder, ChatHeaderPresenceDotEllipse, null);
        lastRenderedChatHeaderSignature = string.Empty;
        // No dialog selected → hide the whole peer header + trust/status bar so the empty
        // detail pane shows a single centered prompt, not a fake "?" avatar + "Не проверен"
        // badge + "диалог не выбран" status all repeating "nothing is selected".
        ChatHeaderGrid.Visibility = Visibility.Collapsed;
        // Nor a message box with nothing to send it to.
        ChatComposerBorder.Visibility = Visibility.Collapsed;
        ChatHeaderStatusBarBorder.Visibility = Visibility.Collapsed;
        ChatIdentityTitleTextBlock.Text = "Диалог";
        ChatIdentitySubtitleTextBlock.Text = "Выберите контакт слева или отправьте запрос.";
        ChatConnectionStatusTextBlock.Text = "Статус: диалог не выбран";
        ChatTrustWarningTextBlock.Text = string.Empty;
        ChatTrustWarningTextBlock.Visibility = Visibility.Collapsed;
        ChatSessionInfoBorder.Visibility = Visibility.Collapsed;
        ChatSessionStateTextBlock.Text = "Сессия: ожидание";
        ChatSessionLatencyTextBlock.Text = "Пинг: — • Keepalive: —";
        ChatSessionNetworkTextBlock.Text = "Сеть: — • Endpoint: —";
        endChatSessionMenuItem.IsEnabled = false;
        DeleteChatContactButton.Visibility = Visibility.Collapsed;
        ChatSecuritySummaryTextBlock.Text = "Сведения о шифровании появятся после выбора или запуска диалога.";
        SecurityDiagnosticsTextBlock.Text = "Сводка безопасности для текущего диалога недоступна.";
        PrekeyBundleStatusTextBlock.Text = "Статус: диалог не выбран";
        RenderCurrentChatCallSummary();
        UpdateCurrentPeerText();
        MessagesListView.Visibility = Visibility.Collapsed;
        MessagesEmptyStateTitleTextBlock.Text = ChatPresentationText.BuildTranscriptSelectionHeading(chatThreadItems.Count > 0);
        MessagesEmptyStateTextBlock.Text = ChatPresentationText.BuildTranscriptChatSelectionPrompt(chatThreadItems.Count > 0);
        MessagesEmptyStateBorder.Visibility = Visibility.Visible;
    }
    private void ApplyChatHeaderRefreshPayload(ChatHeaderRefreshPayload payload)
    {
        if (!string.Equals(windowState.CurrentPeerKey, payload.DialogKey, StringComparison.Ordinal))
        {
            windowState.CurrentPeerKey = payload.DialogKey;
        }
        if (!string.Equals(windowState.CurrentTransportPeerKey, payload.DialogKey, StringComparison.Ordinal))
        {
            windowState.CurrentTransportPeerKey = payload.DialogKey;
        }

        string signature = ChatHeaderSignature.Build(
            payload.HeaderSummary,
            payload.SecuritySummary,
            payload.SessionSummary,
            payload.CallSummary);
        if (string.Equals(signature, lastRenderedChatHeaderSignature, StringComparison.Ordinal))
        {
            RenderChatRemoteControlAffordance(); // online state moves without the header changing
            currentChatSessionSummary = payload.SessionSummary;
            currentChatCallSummary = ResolveDisplayedChatCallSummary(payload.DialogKey, payload.CallSummary);
            RenderChatSessionSummary(currentChatSessionSummary);
            RenderCurrentChatCallSummary();
            endChatSessionMenuItem.IsEnabled = CanEndCurrentChatSession();
            return;
        }

        currentChatHeaderSummary = payload.HeaderSummary;
        currentChatSecuritySummary = payload.SecuritySummary;
        currentChatSessionSummary = payload.SessionSummary;
        currentChatCallSummary = ResolveDisplayedChatCallSummary(payload.DialogKey, payload.CallSummary);
        ApplyAvatarVisual(
            ChatIdentityAvatarImage,
            ChatIdentityAvatarTextBlock,
            payload.HeaderSummary.ContactCard?.Contact.PublicAvatarImagePath,
            MainWindowPresentation.BuildAvatarText(payload.HeaderSummary.ContactCard?.Contact is { } named
                ? MainWindowPresentation.ContactTitle(named)
                : payload.HeaderSummary.Title),
            loadingRing: ChatIdentityAvatarLoadingRing,
            isLoading: payload.HeaderSummary.ContactCard?.Contact is not null &&
                !string.IsNullOrWhiteSpace(payload.HeaderSummary.ContactCard.Contact.PublicAvatarPendingHashHex),
            versionToken: BuildContactAvatarVersionToken(payload.HeaderSummary.ContactCard?.Contact),
            circle: ChatIdentityAvatarCircle,
            colorSeed: payload.HeaderSummary.ContactCard?.Contact is { } seeded
                ? MainWindowPresentation.AvatarSeed(seeded)
                : payload.DialogKey);
        transcriptPeer.Set(
            ChatIdentityAvatarTextBlock.Text,
            ChatIdentityAvatarImage.Source,
            payload.HeaderSummary.ContactCard?.Contact is { } peer ? MainWindowPresentation.AvatarSeed(peer) : payload.DialogKey);
        // Точку и слово присутствия считаем ОДИН раз и вместе: они — две проекции одного решения.
        // Раньше слово приходило из Rust'а (из объявленного присутствия), а точка рисовалась здесь
        // (из живого линка), и на экране «В сети» стояло рядом с оранжевой точкой.
        ContactPresenceDotKind headerPresenceKind = ApplyPresenceDot(
            ChatHeaderPresenceDotBorder,
            ChatHeaderPresenceDotEllipse,
            payload.HeaderSummary.ContactCard?.Contact);
        // A dialog is selected → reveal the peer header + trust/status bar (collapsed in the
        // no-selection empty state by ClearChatHeaderSummaryState).
        ChatHeaderGrid.Visibility = Visibility.Visible;
        ChatComposerBorder.Visibility = Visibility.Visible;
        ChatHeaderStatusBarBorder.Visibility = Visibility.Visible;
        RenderChatRemoteControlAffordance();
        ChatIdentityTitleTextBlock.Text = payload.HeaderSummary.ContactCard?.Contact is { } titled
            ? MainWindowPresentation.ContactTitle(titled)
            : currentChatHeaderSummary.Title;
        // ONE status in the header (owner, 2026-10-10): the line under the name says "В сети ·
        // напрямую · QUIC" itself (route and carrier: the owner wants to see QUIC vs TCP-дверь at a
        // glance), and the separate status line only appears when there is no link — then it
        // carries the reason. Trust is said there too, and only as an alarm ("· идентичность
        // изменилась"); "Never met" is the presence word itself. A "Проверен" chip on every
        // working chat said nothing.
        (string? routeWords, string? linkLine, bool online) =
            DescribeHeaderLink(currentChatHeaderSummary.ContactCard?.Contact.RemoteAccountId);
        string subtitle = ComposeChatHeaderSubtitle(
            currentChatHeaderSummary.Subtitle,
            payload.HeaderSummary.ContactCard?.Contact,
            headerPresenceKind);
        if (online && routeWords is not null)
        {
            subtitle = $"{subtitle} · {routeWords}";
        }

        string? trustWords = DescribeHeaderTrustProblem(currentChatHeaderSummary.ContactCard?.TrustState);
        ChatIdentitySubtitleTextBlock.Text = trustWords is null ? subtitle : $"{subtitle} · {trustWords}";
        ToolTipService.SetToolTip(ChatIdentitySubtitleTextBlock, linkLine);
        string connectionSecurityStatus =
            MainWindowPresentation.BuildChatConnectionStatus(currentChatSecuritySummary, currentChatHeaderSummary.ContactCard);
        // Prefer the honest transport link-state (direct / via-relay / searching / cannot-reach
        // [reason]) for the current contact; fall back to the security/session-derived status when
        // there's no resolvable contact. See docs/decisions/omni-network-model.md.
        ChatConnectionStatusTextBlock.Text = linkLine ?? connectionSecurityStatus;
        ChatConnectionStatusTextBlock.Visibility = online ? Visibility.Collapsed : Visibility.Visible;
        // "Подключиться" — the explicit twin of the auto reconnect-on-open nudge. Shown only when
        // this is an approved contact with a remote account that has no live mesh link right now
        // (offline / still searching), so the user can force an immediate rendezvous re-find instead
        // of waiting for the periodic sweep. Auto-reconnect still runs regardless; this is control +
        // visibility, and it self-hides once the link comes up (linked contacts don't show it).
        UpdateReconnectButtonVisibility();
        string? warningText = currentChatHeaderSummary.ContactCard is null
            ? "Чтобы отправлять сообщения, добавьте или привяжите контакт в разделе «Контакты»."
            : string.Equals(currentChatHeaderSummary.ContactCard.Contact.RelationshipState, "declined", StringComparison.OrdinalIgnoreCase)
                ? "Контакт отклонён. Повторные запросы будут игнорироваться, пока вы не примете решение заново."
                : string.Equals(currentChatHeaderSummary.ContactCard.Contact.RelationshipState, "pending", StringComparison.OrdinalIgnoreCase)
                    ? "Запрос на добавление ещё не решён. Пока можно только принять или отклонить контакт."
                    : currentChatHeaderSummary.ContactCard.Contact.RemoteAccountId is null
                        ? "Контакт ещё не подтверждён. Проверку можно завершить позже."
                        : null;
        if (IsCurrentChatAwaitingFirstConnection())
        {
            warningText = "Контакт сохранён — ждём, когда он появится в сети.";
        }
        ChatTrustWarningTextBlock.Text = warningText ?? string.Empty;
        ChatTrustWarningTextBlock.Visibility = string.IsNullOrWhiteSpace(warningText)
            ? Visibility.Collapsed
            : Visibility.Visible;
        UpdateChatStatusBarVisibility();
        RenderChatSessionSummary(currentChatSessionSummary);
        RenderCurrentChatCallSummary();
        endChatSessionMenuItem.IsEnabled = CanEndCurrentChatSession();
        DeleteChatContactButton.Visibility = currentChatHeaderSummary.ContactCard is null
            ? Visibility.Collapsed
            : Visibility.Visible;
        ChatHeaderProfileButton.IsHitTestVisible = currentChatHeaderSummary.ContactCard is not null;
        ChatSecuritySummaryTextBlock.Text =
            MainWindowPresentation.BuildChatSecuritySummary(currentChatSecuritySummary, currentChatHeaderSummary.ContactCard);
        SecurityDiagnosticsTextBlock.Text =
            MainWindowPresentation.BuildSecurityDiagnostics(currentChatSecuritySummary, currentChatHeaderSummary.ContactCard);
        PrekeyBundleStatusTextBlock.Text = connectionSecurityStatus;
        UpdateCurrentPeerText();
        lastRenderedChatHeaderSignature = signature;
    }

    // Honest per-contact transport link-state for the chat header — fetched from the connection-
    // state DTO (Rust = truth). `RouteWords` is the part for the subtitle ("напрямую · QUIC" / "через
    // сеть · TCP-дверь"), `LinkLine` the full line (carrier included), `Online` whether there is a live link.
    // Nulls when there's no resolvable contact / mesh not started / transient: the caller falls back
    // to the security-derived status.
    private (string? RouteWords, string? LinkLine, bool Online) DescribeHeaderLink(string? remoteAccountId)
    {
        if (string.IsNullOrWhiteSpace(remoteAccountId))
        {
            return (null, null, false);
        }

        try
        {
            MeshConnectionStateDto state = rustBridge.MeshConnectionState();
            ContactConnectionDto? contact = state.Contacts
                .FirstOrDefault(c => string.Equals(c.AccountId, remoteAccountId, StringComparison.Ordinal));
            if (contact is null)
            {
                return (null, null, false);
            }

            string formatted = FormatContactLinkState(contact, state.Node.Kind);
            string? route = contact.Link switch
            {
                "online_direct" => $"напрямую{FormatCarrier(contact.Carrier)}",
                "online_via_relay" => $"через сеть{FormatCarrier(contact.Carrier)}",
                _ => null,
            };
            return (route, string.IsNullOrEmpty(formatted) ? null : formatted, route is not null);
        }
        catch
        {
            return (null, null, false); // mesh not started / transient
        }
    }

    // The header's trust word: only an alarm (the key changed, re-verify). "Never met" is already
    // the presence word for a contact without a verified session, and a plain "не проверен" beside
    // every working chat would be noise again (owner, 2026-10-10).
    private static string? DescribeHeaderTrustProblem(string? trustState)
        => trustState?.ToLowerInvariant() switch
        {
            "reverifyneeded" or "reverify_needed" or "identitychanged" or "identity_changed"
                => MainWindowPresentation.MapTrustState(trustState).ToLowerInvariant(),
            _ => null,
        };

    // Map a contact's link-state DTO to a short, honest chat-header line. Trust (`Verified`/has_met)
    // is orthogonal to the link, surfaced as a "не проверен" suffix on a live link (TM-012).
    private static string FormatContactLinkState(ContactConnectionDto contact, string nodeKind)
    {
        string unverified = contact.Verified ? string.Empty : " · 🛡 не проверен";
        string carrier = FormatCarrier(contact.Carrier);
        return contact.Link switch
        {
            "online_direct" => $"● Онлайн, напрямую{carrier}{unverified}",
            "online_via_relay" => contact.RelayHops is int hops
                ? $"◐ Онлайн, через сеть ({hops} {HopWord(hops)}){carrier}{unverified}"
                : $"◐ Онлайн, через сеть{carrier}{unverified}",
            "searching" => FormatSearchingStage(nodeKind),
            "will_deliver_later" => "✉ Доставлю, когда появится",
            "cannot_reach" => $"⚠ {FormatCannotReachReason(contact.CannotReachReason)}",
            _ => string.Empty,
        };
    }

    // The honest carrier suffix: QUIC (native UDP, fast) vs TCP-фолбэк (QUIC tunnelled over TLS-over-
    // TCP when UDP is blocked/throttled — slower) vs LAN. Empty when the carrier is unknown/absent.
    // Несущая, которой линк идёт НА САМОМ ДЕЛЕ. `tcp_door` — это по-прежнему ПРЯМОЙ путь (дверь не
    // посредник), но байты едут по TCP, а не по UDP; до 2026-08-31 этот случай приезжал сюда как
    // `quic`, и шапка подписывала «· QUIC» поверх TCP-соединения.
    private static string FormatCarrier(string? carrier) => carrier switch
    {
        "quic" => " · QUIC",
        "tcp_door" => " · TCP-дверь",
        "tcp_fallback" => " · TCP-фолбэк",
        "lan" => " · LAN",
        _ => string.Empty,
    };

    // The account-ids we hold a LIVE mesh link to right now (online_direct / online_via_relay), from
    // the honest connection-state DTO. The presence dot uses this as the single source of "online"
    // (green) — so the dot can't say green while the link is actually down (a peer that went offline).
    // One FFI read per render; empty on any error (mesh not started / transient).
    private HashSet<string> CurrentlyLinkedAccountIds()
    {
        try
        {
            return rustBridge.MeshConnectionState().Contacts
                .Where(c => c.Link is "online_direct" or "online_via_relay")
                .Select(c => c.AccountId)
                .ToHashSet(StringComparer.Ordinal);
        }
        catch
        {
            return new HashSet<string>(StringComparer.Ordinal);
        }
    }

    // Show the "Подключиться" button only for an approved contact (has a remote account) that has NO
    // live mesh link right now — i.e. offline / still searching. A linked contact needs no button;
    // a pending/declined/unverified one can't be dialed yet. The auto reconnect-on-open nudge already
    // fired; this is the explicit, visible control for the same core action (rendezvous re-find).
    private void UpdateReconnectButtonVisibility()
    {
        string? accountId = currentChatHeaderSummary?.ContactCard?.Contact.RemoteAccountId;
        bool approved = string.Equals(
            currentChatHeaderSummary?.ContactCard?.Contact.RelationshipState,
            "approved",
            StringComparison.OrdinalIgnoreCase);
        bool linked = !string.IsNullOrWhiteSpace(accountId)
            && CurrentlyLinkedAccountIds().Contains(accountId);
        ReconnectContactButton.Visibility = approved && !string.IsNullOrWhiteSpace(accountId) && !linked
            ? Visibility.Visible
            : Visibility.Collapsed;
        UpdateChatStatusBarVisibility();
    }

    // The bar under the header is for trouble only (no link, a warning, "Подключиться"): a
    // working chat shows none of it, not an empty strip with a rule.
    private void UpdateChatStatusBarVisibility()
    {
        bool any = ChatConnectionStatusTextBlock.Visibility == Visibility.Visible
            || ChatTrustWarningTextBlock.Visibility == Visibility.Visible
            || ReconnectContactButton.Visibility == Visibility.Visible;
        ChatHeaderStatusBarBorder.Visibility = ChatHeaderGrid.Visibility == Visibility.Visible && any
            ? Visibility.Visible
            : Visibility.Collapsed;
    }

    // Honest "how the connection is happening right now" for the searching state — derived from this
    // node's own connectivity (Rust = truth). There's no per-rung live trace yet (that needs dial-
    // ladder instrumentation), so this names what the autoconnect worker is actually doing at this
    // stage instead of a bare "ищу".
    private static string FormatSearchingStage(string nodeKind) => nodeKind switch
    {
        "not_started" => "⏳ Сеть не запущена",
        "isolated" => "⏳ Нет сети — ищу в локальной сети…",
        "connecting" => "⏳ Подключаюсь к сети…",
        // online (reachable or behind NAT): announce + rendezvous is the live mechanism.
        // How (announces, the rendezvous beacon) is the trace under «Подробнее», not the status.
        _ => "⏳ Ищу собеседника в сети…",
    };

    // Contact-card connection (maintainers, 2026-10-10: «карточка — один статус»). The line under the name
    // is the chat header's: "В сети · напрямую · QUIC", or the relationship / the reason there is no
    // link. The rest — the full link line and the Tier-2 journey (how it is connecting, where it
    // stalled) — waits under «Подробнее о соединении». Only an actionable warning (no courier in
    // common) stays in plain sight. One DTO fetch (cheap, in-memory read-model).
    private void RefreshSelectedContactConnection(long nowUnixMs)
    {
        ContactSnapshot? selected = selectedContactCard?.Contact;
        SelectedContactSubtitleTextBlock.Text = selected is null
            ? contactItems.Count == 0 ? "Добавьте первого — по приглашению, коду связи или Omni-ID." : "Слева — все ваши контакты."
            : MainWindowPresentation.BuildContactStatus(selected, nowUnixMs, CurrentlyLinkedAccountIds());
        SelectedContactConnectionTextBlock.Text = string.Empty;
        SelectedContactConnectionDetailTextBlock.Text = string.Empty;
        SelectedContactConnectionTraceTextBlock.Text = string.Empty;
        string? accountId = selected?.RemoteAccountId;
        try
        {
            if (selected is null || string.IsNullOrWhiteSpace(accountId))
            {
                return;
            }

            MeshConnectionStateDto state = rustBridge.MeshConnectionState();
            ContactConnectionDto? contact = state.Contacts
                .FirstOrDefault(c => string.Equals(c.AccountId, accountId, StringComparison.Ordinal));
            if (contact is null)
            {
                return;
            }

            string linkLine = FormatContactLinkState(contact, state.Node.Kind);
            string? route = contact.Link switch
            {
                "online_direct" => $"напрямую{FormatCarrier(contact.Carrier)}",
                "online_via_relay" => $"через сеть{FormatCarrier(contact.Carrier)}",
                _ => null,
            };
            // Under the name only the status («В сети», «Ещё не встречались»); how we are linked is
            // the «Соединение» row below, as a profile in Telegram keeps the name line short.
            string connection = route is not null
                ? char.ToUpper(route[0]) + route[1..]
                : string.Equals(selected.RelationshipState, "approved", StringComparison.OrdinalIgnoreCase) && !string.IsNullOrEmpty(linkLine)
                    // No link: the reason (searching / will deliver later / cannot reach).
                    ? linkLine.TrimStart('⏳', '✉', '⚠', ' ')
                    : string.Empty;
            SelectedContactConnectionTextBlock.Text = $"{connection}{FormatCourierGap(contact)}".Trim();
            SelectedContactConnectionDetailTextBlock.Text = string.IsNullOrEmpty(linkLine) ? string.Empty : $"Соединение: {linkLine}";
            SelectedContactConnectionTraceTextBlock.Text = FormatConnectionTrace(contact.Trace);
        }
        catch
        {
            // Mesh not started / transient — the status stays the relationship one, details blank.
        }
        finally
        {
            SelectedContactConnectionTextBlock.Visibility = string.IsNullOrEmpty(SelectedContactConnectionTextBlock.Text)
                ? Visibility.Collapsed
                : Visibility.Visible;
            SelectedContactConnectionDetailsExpander.Visibility =
                string.IsNullOrEmpty(SelectedContactConnectionDetailTextBlock.Text) && string.IsNullOrEmpty(SelectedContactConnectionTraceTextBlock.Text)
                    ? Visibility.Collapsed
                    : Visibility.Visible;
            // Nothing to say about the link (no account yet, mesh off): no empty «Соединение» row.
            SelectedContactConnectionRow.Visibility =
                SelectedContactConnectionTextBlock.Visibility == Visibility.Visible
                || SelectedContactConnectionDetailsExpander.Visibility == Visibility.Visible
                    ? Visibility.Visible
                    : Visibility.Collapsed;
        }
    }

    // No courier in common with the contact's device one way or the other: say what it means and what to
    // turn on (docs/decisions/courier-metadata-and-quota.md, step 6).
    private static string FormatCourierGap(ContactConnectionDto contact)
    {
        if (!contact.NoCommonCourier)
        {
            return string.Empty;
        }

        string hint = contact.CouriersToEnable is { Count: > 0 } urls
            ? " Включите в «Настройки → Как вас находят»: " + string.Join(", ", urls.Select(CourierHost)) + "."
            : " У вас нет способа встречи, которым пользуется этот контакт.";
        return "\n⚠ Нет общего способа встречи с этим контактом — найти друг друга получится, только пока вы уже соединены." + hint;
    }

    private static string CourierHost(string url) =>
        url.StartsWith("dht://", StringComparison.Ordinal) ? "Mainline"
        : Uri.TryCreate(url, UriKind.Absolute, out Uri? parsed) ? parsed.Authority : url;

    // The Tier-2 trace as a short multiline journey — each step a bullet with a relative time, newest
    // last. Shows the last few steps so "how it's connecting / where it stalled" reads at a glance.
    private static string FormatConnectionTrace(IReadOnlyList<ConnectionStepDto>? trace)
    {
        if (trace is null || trace.Count == 0)
        {
            return string.Empty;
        }

        long nowMs = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
        var lines = new List<string>();
        foreach (ConnectionStepDto step in trace.Skip(Math.Max(0, trace.Count - 4)))
        {
            string icon = step.Status switch
            {
                "ok" => "✓",
                "failed" => "✕",
                "skipped" => "–",
                _ => "•",
            };
            lines.Add($"{icon} {BuildConnectionStepText(step)} · {FormatRelativeTime(nowMs - step.AtMs)}");
        }

        return string.Join("\n", lines);
    }

    private static string BuildConnectionStepText(ConnectionStepDto step)
    {
        string baseText = step.Stage switch
        {
            "lan" => "Локальная сеть",
            "rendezvous" => "Rendezvous",
            "linked" => "Соединение установлено",
            _ => step.Stage,
        };
        return string.IsNullOrWhiteSpace(step.Detail) ? baseText : $"{baseText}: {step.Detail}";
    }

    private static string FormatRelativeTime(long deltaMs)
    {
        long sec = Math.Max(0, deltaMs) / 1000;
        if (sec < 5) return "сейчас";
        if (sec < 60) return $"{sec}с назад";
        long min = sec / 60;
        if (min < 60) return $"{min}м назад";
        return $"{min / 60}ч назад";
    }

    // Honest "why a connection failed" reason text. `no_route_both_unreachable` IS derived by the
    // core (Phase C: is_symmetric_pair + any_relay_reachable) — the old note claiming otherwise made
    // the banner look impossible while users were seeing it. Still not derived: address_stale,
    // verification_failed.
    private static string FormatCannotReachReason(string? reason) => reason switch
    {
        "peer_offline" => "Контакт офлайн — сообщения доставлю позже",
        "no_route_both_unreachable" => "Напрямую не соединиться: у обоих закрытые сети — нужен релей",
        "rendezvous_disabled" => "Режим «только прямое» — обменяйтесь кодом связи",
        "address_stale" => "Адрес устарел, ищу заново…",
        "not_approved" => "Контакт не подтверждён",
        "verification_failed" => "Проверка личности не прошла — возможна подмена",
        _ => "Не удаётся подключиться",
    };

    // Russian plural for "хоп" (1 хоп / 2 хопа / 5 хопов), with the 11–14 exception.
    private static string HopWord(int hops)
    {
        if (hops % 100 is >= 11 and <= 14)
        {
            return "хопов";
        }

        return (hops % 10) switch
        {
            1 => "хоп",
            2 or 3 or 4 => "хопа",
            _ => "хопов",
        };
    }

    // Presence dot overlay on an avatar — mirrors the contact-list dot so you can tell at a
    // glance whether the person is online (green) / recently active (amber). Shared by the
    // chat header and the contact detail card. MapContactPresenceDot gates it to approved + met
    // contacts (local-only / pending / never-met have no online state to show).
    /// <summary>
    /// Красит точку и ВОЗВРАЩАЕТ принятое решение, чтобы подпись рядом рисовалась из него же, а не
    /// из второго независимого расчёта. Пока их было два, они расходились прямо на экране.
    /// </summary>
    private ContactPresenceDotKind ApplyPresenceDot(
        Border dotBorder,
        Microsoft.UI.Xaml.Shapes.Ellipse dotEllipse,
        ContactSnapshot? contact)
    {
        if (contact is null)
        {
            dotBorder.Visibility = Visibility.Collapsed;
            return ContactPresenceDotKind.None;
        }

        ContactPresenceDotKind kind = MainWindowPresentation.MapContactPresenceDotKind(
            contact,
            System.DateTimeOffset.UtcNow.ToUnixTimeMilliseconds(),
            CurrentlyLinkedAccountIds());
        (Visibility dotVisibility, Brush dotBrush) = MainWindowPresentation.MapContactPresenceDot(kind);
        dotBorder.Visibility = dotVisibility;
        if (dotVisibility == Visibility.Visible)
        {
            dotEllipse.Fill = dotBrush;
        }

        return kind;
    }

    /// <summary>
    /// Подпись шапки = то, что дало ядро (имя/статус), плюс присутствие, посчитанное ЗДЕСЬ из того
    /// же решения, что и точка. Ядро присутствие больше не пишет: оно знало только объявленное
    /// состояние контакта и про живой линк не знало ничего.
    /// </summary>
    private static string ComposeChatHeaderSubtitle(
        string? coreSubtitle,
        ContactSnapshot? contact,
        ContactPresenceDotKind presenceKind)
    {
        string prefix = (coreSubtitle ?? string.Empty).Trim();
        if (contact is null ||
            !contact.RelationshipState.Equals("approved", StringComparison.OrdinalIgnoreCase))
        {
            return prefix;
        }

        string presence = MainWindowPresentation.MapContactPresenceText(contact, presenceKind);
        return prefix.Length == 0 ? presence : $"{prefix} • {presence}";
    }

    private void ApplyChatHeaderRefreshFailure(Exception ex)
    {
        // A dialog is selected but its header failed to load — keep the header visible so the
        // error message is shown in context (not silently hidden by the empty-state collapse).
        ChatHeaderGrid.Visibility = Visibility.Visible;
        ChatComposerBorder.Visibility = Visibility.Visible;
        ChatHeaderStatusBarBorder.Visibility = Visibility.Visible;
        ChatIdentityTitleTextBlock.Text = "Диалог";
        ChatIdentitySubtitleTextBlock.Text = ex.Message;
        ApplyAvatarVisual(
            ChatIdentityAvatarImage,
            ChatIdentityAvatarTextBlock,
            null,
            "?",
            loadingRing: ChatIdentityAvatarLoadingRing,
            isLoading: false,
            versionToken: null);
        ChatConnectionStatusTextBlock.Text = "Статус: недоступно";
        ChatConnectionStatusTextBlock.Visibility = Visibility.Visible;
        ChatTrustWarningTextBlock.Text = string.Empty;
        ChatTrustWarningTextBlock.Visibility = Visibility.Collapsed;
        ChatSessionInfoBorder.Visibility = Visibility.Collapsed;
        ChatSessionStateTextBlock.Text = "Сессия: ожидание";
        ChatSessionLatencyTextBlock.Text = "Пинг: — • Keepalive: —";
        ChatSessionNetworkTextBlock.Text = "Сеть: — • Endpoint: —";
        endChatSessionMenuItem.IsEnabled = false;
        DeleteChatContactButton.Visibility = Visibility.Collapsed;
        ChatSecuritySummaryTextBlock.Text = "Сведения о шифровании появятся после выбора или запуска диалога.";
        SecurityDiagnosticsTextBlock.Text = $"Сводка безопасности недоступна: {ex.Message}";
        PrekeyBundleStatusTextBlock.Text = $"Статус: {NormalizeUserError(ex)}";
        currentChatHeaderSummary = null;
        currentChatSecuritySummary = null;
        currentChatSessionSummary = null;
        currentChatCallSummary = null;
        lastRenderedChatHeaderSignature = string.Empty;
        RenderCurrentChatCallSummary();
        UpdateCurrentPeerText();
    }
    private void RenderChatSessionSummary(ChatSessionSummary? summary)
    {
        if (summary is null)
        {
            ChatSessionInfoBorder.Visibility = Visibility.Collapsed;
            ChatSessionStateTextBlock.Text = "Сессия: ожидание";
            ChatSessionLatencyTextBlock.Text = "Пинг: — • Keepalive: —";
            ChatSessionNetworkTextBlock.Text = "Сеть: — • Endpoint: —";
            return;
        }

        long nowUnixMs = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
        IReadOnlyList<NetworkInterfaceSnapshot> inventory = rustBridge.GetLocalNetworkInventory();
        string networkLabel = MainWindowPresentation.BuildChatSessionNetworkLabel(summary, inventory);
        string endpointText = MainWindowPresentation.BuildChatSessionEndpointText(summary);

        ChatSessionInfoBorder.Visibility = Visibility.Visible;
        string? peerDevice = MainWindowPresentation.BuildChatSessionPeerDeviceText(summary, inventory);
        ChatSessionStateTextBlock.Text = $"Сессия: {MainWindowPresentation.MapChatSessionState(summary.SessionStateValue)}"
            + (peerDevice is null ? string.Empty : $" • {peerDevice}");
        ChatSessionLatencyTextBlock.Text =
            $"{MainWindowPresentation.BuildChatSessionPingText(summary)} • {MainWindowPresentation.BuildChatSessionKeepaliveText(summary, nowUnixMs)}";
        ChatSessionNetworkTextBlock.Text = $"Сеть: {networkLabel} • {endpointText}";
    }

    private bool CanEndCurrentChatSession()
    {
        if (currentChatHeaderSummary?.ContactCard is null || IsCurrentChatAwaitingFirstConnection())
        {
            return false;
        }

        if (!string.Equals(
                currentChatHeaderSummary.ContactCard.Contact.RelationshipState,
                "approved",
                StringComparison.OrdinalIgnoreCase))
        {
            return false;
        }

        if (currentChatSessionSummary is null)
        {
            return false;
        }

        return !string.Equals(currentChatSessionSummary.SessionStateValue, "paused", StringComparison.OrdinalIgnoreCase) &&
               (!string.Equals(currentChatSessionSummary.SessionStateValue, "idle", StringComparison.OrdinalIgnoreCase) ||
                !string.IsNullOrWhiteSpace(currentChatSessionSummary.PeerEndpoint) ||
                !string.IsNullOrWhiteSpace(currentChatSessionSummary.LocalEndpoint) ||
                currentChatSessionSummary.IsOpenLease);
    }

    private ChatThreadListItem? ResolveChatSelectionFallback(string? requestedDialogKey)
    {
        if (string.IsNullOrWhiteSpace(requestedDialogKey))
        {
            return null;
        }

        string? fallbackContactId =
            selectedContactCard?.Contact.ContactId ??
            currentChatHeaderSummary?.ContactCard?.Contact.ContactId ??
            windowState.SelectedContactId;
        if (string.IsNullOrWhiteSpace(fallbackContactId))
        {
            return null;
        }

        return chatThreadItems.FirstOrDefault(item =>
            string.Equals(item.ContactId, fallbackContactId, StringComparison.Ordinal));
    }

    private bool IsCurrentChatAwaitingFirstConnection()
    {
        ContactSnapshot? contact = currentChatHeaderSummary?.ContactCard?.Contact;
        return contact is not null &&
               string.Equals(contact.RelationshipState, "approved", StringComparison.OrdinalIgnoreCase) &&
               IsAccountDialogKey(currentChatHeaderSummary?.DialogKey) &&
               string.IsNullOrWhiteSpace(contact.RemoteInstanceId);
    }
}
