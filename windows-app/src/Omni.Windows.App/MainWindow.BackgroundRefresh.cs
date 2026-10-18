using System.Diagnostics;
using System.Threading;
using Omni.Windows.App.Core;
using Omni.Windows.App.ViewModels;

namespace Omni.Windows.App;

public sealed partial class MainWindow
{
    [Flags]
    private enum BackgroundRefreshDomain
    {
        None = 0,
        Lifecycle = 1 << 0,
        Transport = 1 << 1,
        Call = 1 << 3,
        ChatHeader = 1 << 4,
        ChatThreads = 1 << 5,
        Messages = 1 << 6,
        Queue = 1 << 7,
        Contacts = 1 << 8,
        SelectedContact = 1 << 9,
        Settings = 1 << 12,
        Footprint = 1 << 13,
        All = Lifecycle |
              Transport |
              Call |
              ChatHeader |
              ChatThreads |
              Messages |
              Queue |
              Contacts |
              SelectedContact |
              Settings |
              Footprint,
    }

    internal static bool SectionShowsTransportDiscoverySurface(AuthenticatedShellSection section)
        => section is AuthenticatedShellSection.Contacts
            or AuthenticatedShellSection.Settings
            or AuthenticatedShellSection.Diagnostics;

    internal static bool SectionShowsQueueSurface(AuthenticatedShellSection section)
        => section is AuthenticatedShellSection.Chats
            or AuthenticatedShellSection.Diagnostics;

    internal static bool SectionShowsContactsSurface(AuthenticatedShellSection section)
        => section == AuthenticatedShellSection.Contacts;

    internal static bool SectionShowsSettingsSurface(AuthenticatedShellSection section)
        => section == AuthenticatedShellSection.Settings;

    internal static bool SectionShowsFootprintSurface(AuthenticatedShellSection section)
        => section is AuthenticatedShellSection.Settings
            or AuthenticatedShellSection.Diagnostics;

    private bool IsAuthenticatedRefreshLoopRunning()
    {
        lock (refreshLoopSync)
        {
            return refreshLoopCancellation is not null;
        }
    }

    private void StopAuthenticatedRefreshLoop()
    {
        CancellationTokenSource? loopCancellation;
        lock (refreshLoopSync)
        {
            loopCancellation = refreshLoopCancellation;
            refreshLoopCancellation = null;
            pendingRefreshWakeDomains = BackgroundRefreshDomain.None;
        }

        loopCancellation?.Cancel();
        ReleaseRefreshLoopWakeSignal();
        Interlocked.Increment(ref refreshLoopGeneration);
        Interlocked.Exchange(ref refreshDispatchQueued, 0);
        refreshCoordinator.CancelPending();
    }

    private void StartAuthenticatedRefreshLoop()
    {
        lock (refreshLoopSync)
        {
            if (refreshLoopCancellation is not null)
            {
                return;
            }

            refreshLoopCancellation = new CancellationTokenSource();
            _ = RunAuthenticatedRefreshLoopAsync(refreshLoopCancellation);
            StartCallChangeWatcher();
        }
    }

    private async Task RunAuthenticatedRefreshLoopAsync(CancellationTokenSource loopCancellation)
    {
        try
        {
            while (!loopCancellation.Token.IsCancellationRequested)
            {
                TimeSpan delay = GetAuthenticatedRefreshLoopDelay();
                if (delay > TimeSpan.Zero)
                {
                    bool wokeEarly = await refreshLoopWakeSignal.WaitAsync(delay, loopCancellation.Token);
                    if (!wokeEarly && loopCancellation.Token.IsCancellationRequested)
                    {
                        break;
                    }
                }

                if (loopCancellation.Token.IsCancellationRequested)
                {
                    break;
                }

                if (Interlocked.CompareExchange(ref refreshDispatchQueued, 1, 0) != 0)
                {
                    await Task.Delay(TimeSpan.FromMilliseconds(50), loopCancellation.Token);
                    continue;
                }

                bool enqueued = DispatcherQueue.TryEnqueue(() =>
                {
                    if (loopCancellation.Token.IsCancellationRequested)
                    {
                        Interlocked.Exchange(ref refreshDispatchQueued, 0);
                        return;
                    }

                    try
                    {
                        if (TryReturnToOnboardingIfProfileWasErased())
                        {
                            return;
                        }

                        RefreshNewDevicesBadgeIfDue();
                        QueueBackgroundRefreshIfNeeded();
                    }
                    catch (Exception ex)
                    {
                        Debug.WriteLine($"Periodic refresh failed: {ex}");
                    }
                    finally
                    {
                        Interlocked.Exchange(ref refreshDispatchQueued, 0);
                    }
                });

                if (!enqueued)
                {
                    Interlocked.Exchange(ref refreshDispatchQueued, 0);
                    break;
                }
            }
        }
        catch (OperationCanceledException)
        {
            // Loop shutdown is expected during logout, mode changes, or window close.
        }
        finally
        {
            lock (refreshLoopSync)
            {
                if (ReferenceEquals(refreshLoopCancellation, loopCancellation))
                {
                    refreshLoopCancellation = null;
                }
            }

            loopCancellation.Dispose();
        }
    }

    private void WakeAuthenticatedRefreshLoop(BackgroundRefreshDomain domains = BackgroundRefreshDomain.None)
    {
        lock (refreshLoopSync)
        {
            if (domains != BackgroundRefreshDomain.None)
            {
                pendingRefreshWakeDomains |= domains;
            }

            if (refreshLoopCancellation is null)
            {
                return;
            }
        }

        ReleaseRefreshLoopWakeSignal();
    }

    private void WakeShellSectionRefresh(AuthenticatedShellSection section)
        => WakeAuthenticatedRefreshLoop(section switch
        {
            AuthenticatedShellSection.Chats => BackgroundRefreshDomain.Call |
                                               BackgroundRefreshDomain.ChatHeader |
                                               BackgroundRefreshDomain.ChatThreads |
                                               BackgroundRefreshDomain.Messages |
                                               BackgroundRefreshDomain.Queue,
            AuthenticatedShellSection.Contacts => BackgroundRefreshDomain.Call |
                                                  BackgroundRefreshDomain.Transport |
                                                  BackgroundRefreshDomain.Contacts |
                                                  BackgroundRefreshDomain.SelectedContact,
            AuthenticatedShellSection.Settings => BackgroundRefreshDomain.Call |
                                                  BackgroundRefreshDomain.Settings |
                                                  BackgroundRefreshDomain.Transport |
                                                  BackgroundRefreshDomain.Footprint,
            AuthenticatedShellSection.Diagnostics => BackgroundRefreshDomain.Call |
                                                     BackgroundRefreshDomain.Transport |
                                                     BackgroundRefreshDomain.Queue |
                                                     BackgroundRefreshDomain.Footprint,
            _ => BackgroundRefreshDomain.None,
        });

    private void WakeChatRefresh()
        => WakeAuthenticatedRefreshLoop(
            BackgroundRefreshDomain.Call |
            BackgroundRefreshDomain.ChatHeader |
            BackgroundRefreshDomain.ChatThreads |
            BackgroundRefreshDomain.Messages |
            BackgroundRefreshDomain.Queue);

    private void WakeCallRefresh()
        => WakeAuthenticatedRefreshLoop(BackgroundRefreshDomain.Call);

    private void WakeTransportRefresh()
        => WakeAuthenticatedRefreshLoop(
            BackgroundRefreshDomain.Transport |
            BackgroundRefreshDomain.Contacts);

    private void WakeSelectedContactRefresh()
        => WakeAuthenticatedRefreshLoop(
            BackgroundRefreshDomain.Contacts |
            BackgroundRefreshDomain.SelectedContact);

    private void ReleaseRefreshLoopWakeSignal()
    {
        if (refreshLoopWakeSignal.CurrentCount > 0)
        {
            return;
        }

        try
        {
            refreshLoopWakeSignal.Release();
        }
        catch (SemaphoreFullException)
        {
            // Coalesce repeated wake requests.
        }
    }

    private TimeSpan GetAuthenticatedRefreshLoopDelay()
    {
        if (!startupPolicyService.CanRunAuthenticatedRefresh(
                windowState.Onboarding.ShellMode,
                rustBridge.IsInitialized,
                windowState.IsConnecting))
        {
            return BackgroundRefreshIdleRecheckCadence;
        }

        DateTimeOffset now = DateTimeOffset.UtcNow;
        if (HasPendingRefreshWakeDomains())
        {
            return TimeSpan.Zero;
        }

        // Out of sight (minimized, or hidden to the tray): poll coarsely instead of firing the fine
        // per-domain cadences (e.g. the 350 ms message poll) every tick — cuts idle CPU/battery. The
        // call deadline stays live so an incoming call still surfaces; everything else catches up
        // when the window is back (OnActivated wakes the loop with All domains). It used to be
        // «unfocused», but a window without focus is often in plain sight — beside the one being
        // typed in, on a second screen — and its chat showed a message up to 3 s late (owner,
        // 2026-10-17).
        if (!IsWindowInSight())
        {
            DateTimeOffset coarseDueAt = MinDueAt(now + BackgroundUnfocusedCadence, nextCallRefreshAt);
            return coarseDueAt <= now ? TimeSpan.Zero : coarseDueAt - now;
        }

        DateTimeOffset nextDueAt = GetNextAuthenticatedRefreshDueAt(now);
        return nextDueAt <= now ? TimeSpan.Zero : nextDueAt - now;
    }

    private bool HasPendingRefreshWakeDomains()
    {
        lock (refreshLoopSync)
        {
            return pendingRefreshWakeDomains != BackgroundRefreshDomain.None;
        }
    }

    private BackgroundRefreshDomain ConsumePendingRefreshWakeDomains()
    {
        lock (refreshLoopSync)
        {
            BackgroundRefreshDomain pending = pendingRefreshWakeDomains;
            pendingRefreshWakeDomains = BackgroundRefreshDomain.None;
            return pending;
        }
    }

    private DateTimeOffset GetNextAuthenticatedRefreshDueAt(DateTimeOffset now)
    {
        AuthenticatedShellSection section = windowState.CurrentSection;
        DateTimeOffset nextDueAt = nextLifecycleRefreshAt;

        if (SectionShowsTransportDiscoverySurface(section))
        {
            nextDueAt = MinDueAt(nextDueAt, nextTransportRefreshAt);
        }

        nextDueAt = MinDueAt(nextDueAt, nextCallRefreshAt);

        switch (section)
        {
            case AuthenticatedShellSection.Chats:
                nextDueAt = MinDueAt(
                    nextDueAt,
                    nextChatHeaderRefreshAt,
                    nextChatListRefreshAt,
                    nextMessageRefreshAt,
                    nextQueueRefreshAt);
                break;
            case AuthenticatedShellSection.Contacts:
                nextDueAt = MinDueAt(nextDueAt, nextContactsRefreshAt);
                break;
            case AuthenticatedShellSection.Diagnostics:
                nextDueAt = MinDueAt(nextDueAt, nextQueueRefreshAt, nextFootprintRefreshAt);
                break;
            case AuthenticatedShellSection.Settings:
                nextDueAt = MinDueAt(nextDueAt, nextSettingsRefreshAt, nextFootprintRefreshAt);
                break;
            default:
                break;
        }

        return nextDueAt == DateTimeOffset.MaxValue
            ? now + BackgroundRefreshIdleRecheckCadence
            : nextDueAt;
    }

    private static DateTimeOffset MinDueAt(DateTimeOffset first, DateTimeOffset second)
        => first <= second ? first : second;

    private static DateTimeOffset MinDueAt(
        DateTimeOffset first,
        DateTimeOffset second,
        DateTimeOffset third)
        => MinDueAt(MinDueAt(first, second), third);

    private static DateTimeOffset MinDueAt(
        DateTimeOffset first,
        DateTimeOffset second,
        DateTimeOffset third,
        DateTimeOffset fourth)
        => MinDueAt(MinDueAt(first, second), MinDueAt(third, fourth));

    private static DateTimeOffset MinDueAt(
        DateTimeOffset first,
        DateTimeOffset second,
        DateTimeOffset third,
        DateTimeOffset fourth,
        DateTimeOffset fifth)
        => MinDueAt(MinDueAt(first, second, third), MinDueAt(fourth, fifth));

    private void QueueBackgroundRefreshIfNeeded(bool forceAll = false)
    {
        BackgroundRefreshRequest? request = BuildBackgroundRefreshRequest(forceAll);
        if (request is null)
        {
            return;
        }

        refreshCoordinator.RequestRefresh(request);
    }

    private BackgroundRefreshRequest? BuildBackgroundRefreshRequest(bool forceAll = false)
    {
        if (!startupPolicyService.CanRunAuthenticatedRefresh(
                windowState.Onboarding.ShellMode,
                rustBridge.IsInitialized,
                windowState.IsConnecting))
        {
            return null;
        }

        DateTimeOffset now = DateTimeOffset.UtcNow;
        AuthenticatedShellSection section = windowState.CurrentSection;
        BackgroundRefreshDomain wakeDomains = forceAll ? BackgroundRefreshDomain.All : ConsumePendingRefreshWakeDomains();
        bool lifecycleDue = forceAll || wakeDomains.HasFlag(BackgroundRefreshDomain.Lifecycle) || now >= nextLifecycleRefreshAt;
        bool transportDue = IsWakeOrVisibleCadenceDue(
            forceAll,
            wakeDomains,
            BackgroundRefreshDomain.Transport,
            SectionShowsTransportDiscoverySurface(section),
            now,
            nextTransportRefreshAt);
        bool callDue = forceAll || wakeDomains.HasFlag(BackgroundRefreshDomain.Call) || now >= nextCallRefreshAt;
        bool chatHeaderDue = forceAll || wakeDomains.HasFlag(BackgroundRefreshDomain.ChatHeader) || now >= nextChatHeaderRefreshAt;
        bool chatListDue = forceAll || wakeDomains.HasFlag(BackgroundRefreshDomain.ChatThreads) || now >= nextChatListRefreshAt;
        bool messageDue = forceAll || wakeDomains.HasFlag(BackgroundRefreshDomain.Messages) || now >= nextMessageRefreshAt;
        bool queueDue = IsWakeOrVisibleCadenceDue(
            forceAll,
            wakeDomains,
            BackgroundRefreshDomain.Queue,
            SectionShowsQueueSurface(section),
            now,
            nextQueueRefreshAt);
        bool contactsDue = IsWakeOrVisibleCadenceDue(
            forceAll,
            wakeDomains,
            BackgroundRefreshDomain.Contacts,
            SectionShowsContactsSurface(section),
            now,
            nextContactsRefreshAt);
        bool settingsDue = IsWakeOrVisibleCadenceDue(
            forceAll,
            wakeDomains,
            BackgroundRefreshDomain.Settings,
            SectionShowsSettingsSurface(section),
            now,
            nextSettingsRefreshAt);
        bool footprintDue = IsWakeOrVisibleCadenceDue(
            forceAll,
            wakeDomains,
            BackgroundRefreshDomain.Footprint,
            SectionShowsFootprintSurface(section),
            now,
            nextFootprintRefreshAt);

        string? dialogKey = GetResolvedCurrentDialogKey();
        bool effectiveChatHeaderDue = false;
        // Every section: the list carries the unread counters, and the «Чаты» rail badge shows them
        // wherever the user is (docs/decisions/unread-counters.md). Cheap — the fetch compares the
        // core's list signature first and reads the threads only when it moved.
        bool effectiveChatListDue = chatListDue;
        bool canRunLiveMessageRefresh =
            !string.IsNullOrWhiteSpace(dialogKey) &&
            activeChatTranscriptState.LiveCursor is not null &&
            transcriptTailLoadCancellation is null;
        bool effectiveMessageDue = false;
        bool effectiveQueueDue = queueDue;
        bool effectiveContactsDue = contactsDue;
        bool effectiveSettingsDue = settingsDue;

        switch (section)
        {
            case AuthenticatedShellSection.Chats:
                effectiveChatHeaderDue = chatHeaderDue;
                effectiveMessageDue = messageDue && canRunLiveMessageRefresh;
                break;
            case AuthenticatedShellSection.Diagnostics:
                // «Открытый чат» (сессия, пинг, keepalive) живёт здесь с 2026-10-10, а питается
                // обновлением шапки чата — без него блок замирал целиком: «Keepalive: 8 сек назад»
                // стояло, пока открыта диагностика.
                effectiveChatHeaderDue = chatHeaderDue;
                break;
            default:
                break;
        }

        bool selectedContactDue = !string.IsNullOrWhiteSpace(windowState.SelectedContactId) &&
                                  (forceAll || wakeDomains.HasFlag(BackgroundRefreshDomain.SelectedContact));

        BackgroundRefreshRequest request = new(
            Volatile.Read(ref refreshLoopGeneration),
            section,
            dialogKey,
            windowState.SelectedContactId,
            windowState.SelectedContactRevision,
            windowState.Onboarding.ActiveProfileId,
            windowState.LastRenderedMessageSignature,
            windowState.LastObservedChatRefreshSignature,
            windowState.LastObservedContactRefreshSignature,
            activeChatTranscriptState.LoadVersion,
            messageItems.Count,
            activeChatTranscriptState.ServerHasOlderMessages,
            activeChatTranscriptState.OldestLoadedCreatedAtUnixMs,
            activeChatTranscriptState.OldestLoadedMessageId,
            lifecycleDue,
            footprintDue,
            transportDue,
            callDue,
            effectiveChatHeaderDue,
            effectiveChatListDue,
            effectiveMessageDue,
            effectiveQueueDue,
            effectiveContactsDue,
            selectedContactDue,
            effectiveSettingsDue);

        if (!request.HasWork)
        {
            return null;
        }

        if (lifecycleDue)
        {
            nextLifecycleRefreshAt = now + LifecycleRefreshCadence;
        }
        if (footprintDue)
        {
            nextFootprintRefreshAt = now + FootprintRefreshCadence;
        }
        if (transportDue)
        {
            nextTransportRefreshAt = now + TransportRefreshCadence;
        }
        if (callDue)
        {
            nextCallRefreshAt = now + GetCallRefreshCadence();
        }
        if (effectiveChatHeaderDue)
        {
            nextChatHeaderRefreshAt = now + ChatHeaderRefreshCadence;
        }
        if (effectiveChatListDue)
        {
            nextChatListRefreshAt = now + ChatListRefreshCadence;
        }
        if (effectiveMessageDue)
        {
            nextMessageRefreshAt = now + MessageRefreshCadence;
        }
        if (effectiveQueueDue)
        {
            nextQueueRefreshAt = now + QueueRefreshCadence;
        }
        if (effectiveContactsDue)
        {
            nextContactsRefreshAt = now + ContactsRefreshCadence;
        }
        if (effectiveSettingsDue)
        {
            nextSettingsRefreshAt = now + SettingsRefreshCadence;
        }

        return request;
    }

    private static bool IsWakeOrVisibleCadenceDue(
        bool forceAll,
        BackgroundRefreshDomain wakeDomains,
        BackgroundRefreshDomain domain,
        bool isVisible,
        DateTimeOffset now,
        DateTimeOffset nextDueAt)
        => forceAll ||
           wakeDomains.HasFlag(domain) ||
           (isVisible && now >= nextDueAt);

    private Task FetchBackgroundRefreshAsync(
        BackgroundRefreshRequest request,
        CancellationToken cancellationToken,
        Action<BackgroundRefreshResult> report)
        => Task.Run(
            () => FetchBackgroundRefresh(request, cancellationToken, report),
            cancellationToken);

    private void FetchBackgroundRefresh(
        BackgroundRefreshRequest request,
        CancellationToken cancellationToken,
        Action<BackgroundRefreshResult> report)
    {
        if (!request.HasWork)
        {
            return;
        }

        LifecycleRefreshPayload? lifecycle = request.LifecycleDue
            ? TryFetchPayload(
                "lifecycle",
                () => FetchLifecycleRefreshPayload(false),
                cancellationToken)
            : null;
        if (lifecycle is not null)
        {
            report(new BackgroundRefreshResult(request.Generation)
            {
                Lifecycle = lifecycle,
            });
        }

        if (request.FootprintDue)
        {
            string? footprintProfileId = lifecycle?.ActiveProfileId ?? request.ActiveProfileId;
            FootprintRefreshPayload? footprint = TryFetchPayload(
                "footprint",
                () => FetchFootprintRefreshPayload(footprintProfileId),
                cancellationToken);
            if (footprint is not null)
            {
                report(new BackgroundRefreshResult(request.Generation)
                {
                    Footprint = footprint,
                });
            }
        }

        TransportDiscoveryRefreshPayload? transportDiscovery = request.TransportDue || request.QueueDue
            ? TryFetchPayload(
                "transport_discovery",
                rustBridge.GetTransportDiscoveryRefreshPayload,
                cancellationToken)
            : null;
        if (transportDiscovery is not null)
        {
            if (request.TransportDue)
            {
                report(new BackgroundRefreshResult(request.Generation)
                {
                    Transport = new TransportRefreshPayload(transportDiscovery.TransportSnapshot),
                    QueueDiagnostics = new QueueDiagnosticsRefreshPayload(
                        transportDiscovery.QueueDiagnosticsSnapshot,
                        request.DialogKey),
                });
            }
            else if (request.QueueDue)
            {
                report(new BackgroundRefreshResult(request.Generation)
                {
                    QueueDiagnostics = new QueueDiagnosticsRefreshPayload(
                        transportDiscovery.QueueDiagnosticsSnapshot,
                        request.DialogKey),
                });
            }
        }

        CallInboxRefreshPayload? callInbox = request.CallDue
            ? TryFetchPayload(
                "call_inbox",
                () => new CallInboxRefreshPayload(rustBridge.GetCallInboxSnapshot()),
                cancellationToken)
            : null;
        if (callInbox is not null)
        {
            report(new BackgroundRefreshResult(request.Generation)
            {
                CallInbox = callInbox,
            });
        }

        ChatHeaderRefreshPayload? chatHeader = request.ChatHeaderDue
            ? TryFetchPayload(
                "chat_header",
                () => chatRefreshService.FetchHeader(rustBridge, request.DialogKey, cancellationToken),
                cancellationToken)
            : null;
        if (chatHeader is not null)
        {
            report(new BackgroundRefreshResult(request.Generation)
            {
                ChatHeader = chatHeader,
            });
        }

        ChatThreadRefreshPayload? chatThreads = request.ChatListDue
            ? TryFetchPayload(
                "chat_threads",
                () => chatRefreshService.FetchThreadList(
                    rustBridge,
                    request.LastObservedChatRefreshSignature,
                    cancellationToken),
                cancellationToken)
            : null;
        if (chatThreads is not null)
        {
            report(new BackgroundRefreshResult(request.Generation)
            {
                ChatThreads = chatThreads,
            });
        }

        MessageRefreshPayload? messages = request.MessageDue
            ? TryFetchPayload(
                "messages",
                () => chatRefreshService.FetchLiveMessages(
                    rustBridge,
                    request.DialogKey,
                    request.LastRenderedMessageSignature,
                    request.TranscriptLoadVersion,
                    request.CurrentTranscriptItemCount,
                    request.CurrentTranscriptServerHasOlderMessages,
                    request.CurrentTranscriptOldestLoadedCreatedAtUnixMs,
                    request.CurrentTranscriptOldestLoadedMessageId,
                    cancellationToken),
                cancellationToken)
            : null;
        if (messages is not null)
        {
            report(new BackgroundRefreshResult(request.Generation)
            {
                Messages = messages,
            });
        }

        ContactsRefreshPayload? contacts = request.ContactsDue
            ? TryFetchPayload(
                "contacts",
                () => FetchContactsRefreshPayload(request.LastObservedContactRefreshSignature),
                cancellationToken)
            : null;
        if (contacts is not null)
        {
            report(new BackgroundRefreshResult(request.Generation)
            {
                Contacts = contacts,
            });
        }

        bool shouldFetchSelectedContact = !string.IsNullOrWhiteSpace(request.SelectedContactId) &&
                                          (request.SelectedContactDue || contacts is not null);
        SelectedContactRefreshPayload? selectedContact = shouldFetchSelectedContact
            ? TryFetchPayload(
                "selected_contact",
                () =>
                {
                    EntityRevisionSnapshot? revisionSnapshot = rustBridge.GetContactCardRevision(request.SelectedContactId!);
                    if (revisionSnapshot is null)
                    {
                        return request.SelectedContactRevision is null && contacts is null
                            ? null
                            : new SelectedContactRefreshPayload(request.SelectedContactId!, null, null);
                    }

                    if (revisionSnapshot.Revision == request.SelectedContactRevision)
                    {
                        return null;
                    }

                    return new SelectedContactRefreshPayload(
                        request.SelectedContactId!,
                        rustBridge.GetContactCard(request.SelectedContactId!),
                        revisionSnapshot.Revision);
                },
                cancellationToken)
            : null;
        if (selectedContact is not null)
        {
            report(new BackgroundRefreshResult(request.Generation)
            {
                SelectedContact = selectedContact,
            });
        }

        PasswordProtectionRefreshPayload? passwordProtection = request.SettingsDue
            ? TryFetchPayload(
                "password_protection",
                () => FetchPasswordProtectionRefreshPayload(request.ActiveProfileId),
                cancellationToken)
            : null;
        if (passwordProtection is not null)
        {
            report(new BackgroundRefreshResult(request.Generation)
            {
                PasswordProtection = passwordProtection,
            });
        }
    }

    private static TPayload? TryFetchPayload<TPayload>(
        string operation,
        Func<TPayload?> fetch,
        CancellationToken cancellationToken)
        where TPayload : class
    {
        cancellationToken.ThrowIfCancellationRequested();

        try
        {
            return fetch();
        }
        catch (Exception ex)
        {
            Debug.WriteLine($"Background refresh operation '{operation}' failed: {ex}");
            return null;
        }
    }

    private void ApplyBackgroundRefresh(BackgroundRefreshResult result)
    {
        if (result.Generation != Volatile.Read(ref refreshLoopGeneration))
        {
            return;
        }

        if (result.Lifecycle is not null)
        {
            ApplyLifecycleRefreshPayload(result.Lifecycle);
        }

        if (result.Footprint is not null)
        {
            ApplyFootprintRefreshPayload(result.Footprint);
        }

        if (result.Transport is not null)
        {
            ApplyTransportRefreshPayload(result.Transport);
            // The open contact card's «Соединение» and connection journey («Rendezvous · 12с назад»)
            // move with the link, not with the card: they were drawn only on selection and froze
            // while the card stayed open. The transport tick is the link's own cadence.
            if (windowState.CurrentSection == AuthenticatedShellSection.Contacts && selectedContactCard is not null)
            {
                RefreshSelectedContactConnection(DateTimeOffset.UtcNow.ToUnixTimeMilliseconds());
            }
        }

        if (result.CallInbox is not null)
        {
            ApplyCallInboxRefreshPayload(result.CallInbox);
        }

        if (result.ChatThreads is not null)
        {
            windowState.LastObservedChatRefreshSignature = result.ChatThreads.ObservedSignature;
            ApplyChatThreadProjections(
                result.ChatThreads.Projections,
                refreshQueueDiagnostics: false,
                groupRows: result.ChatThreads.Groups);
        }

        if (result.ChatHeader is not null &&
            string.Equals(result.ChatHeader.DialogKey, GetResolvedCurrentDialogKey(), StringComparison.Ordinal))
        {
            ApplyChatHeaderRefreshPayload(result.ChatHeader);
        }

        if (result.Messages is not null &&
            result.Messages.LoadVersion == activeChatTranscriptState.LoadVersion)
        {
            ApplyMessageRefreshPayload(result.Messages);
        }

        if (result.QueueDiagnostics is not null)
        {
            ApplyQueueDiagnosticsRefreshPayload(result.QueueDiagnostics);
        }

        if (result.Contacts is not null)
        {
            ApplyContactsRefreshPayload(result.Contacts);
        }

        if (result.SelectedContact is not null &&
            string.Equals(result.SelectedContact.ContactId, windowState.SelectedContactId, StringComparison.Ordinal))
        {
            ApplySelectedContactRefreshPayload(result.SelectedContact);
        }

        if (result.PasswordProtection is not null)
        {
            ApplyPasswordProtectionRefreshPayload(result.PasswordProtection);
        }

        UpdateControlStates();
    }

    private ContactsRefreshPayload? FetchContactsRefreshPayload(string lastObservedSignature)
    {
        ContactsScreenSnapshot snapshot = rustBridge.GetContactsScreenSnapshot();
        // Live links and the clock move presence without touching storage — see the chat list.
        string signature = BuildContactsRefreshSignature(snapshot.RefreshSummary) + ":" +
                           ChatRefreshService.LinkedPresenceSignature(rustBridge) + ":" +
                           (DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() / 60_000);
        if (string.Equals(signature, lastObservedSignature, StringComparison.Ordinal))
        {
            return null;
        }

        return new ContactsRefreshPayload(signature, snapshot.Contacts);
    }

    private void ApplyContactsRefreshPayload(ContactsRefreshPayload payload)
    {
        windowState.LastObservedContactRefreshSignature = payload.ObservedSignature;
        ApplyContactsSnapshots(payload.Contacts);
    }

    private static string BuildContactsRefreshSignature(ContactsRefreshSummary summary)
        => string.Join(
            ':',
            summary.ContactCount,
            summary.LastContactUpdatedAtUnixMs,
            summary.LastPublicProfileUpdatedAtUnixMs,
            summary.LastPresenceUpdatedAtUnixMs,
            summary.LastVerificationUpdatedAtUnixMs,
            summary.LastIdentityObservationUpdatedAtUnixMs,
            summary.LastContactDevicesUpdatedAtUnixMs);
}
