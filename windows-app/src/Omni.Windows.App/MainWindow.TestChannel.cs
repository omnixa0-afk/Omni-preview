using System.Runtime.InteropServices.WindowsRuntime;
using System.Text;
using Microsoft.UI.Xaml;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Calls;
using Omni.Windows.App.Presentation;

namespace Omni.Windows.App;

/// <summary>
/// Commands for <see cref="UiTestChannel"/>. Each one drives the SAME handler the button
/// drives — the point of the channel is to exercise the real path, not a parallel one, so
/// a bug found by a script is a bug the user can hit.
///
/// Everything here runs on the UI thread (the channel marshals) and is dev-only: nothing
/// starts unless OMNI_UI_TEST_CHANNEL is set. See docs/decisions/ui-test-channel.md.
/// </summary>
public sealed partial class MainWindow
{
    private UiTestChannel? uiTestChannel;

    private static FrameworkElement? FindNamedDescendant(DependencyObject node, string name)
    {
        if (node is FrameworkElement element && element.Name == name)
        {
            return element;
        }

        int count = Microsoft.UI.Xaml.Media.VisualTreeHelper.GetChildrenCount(node);
        for (int i = 0; i < count; i++)
        {
            if (FindNamedDescendant(Microsoft.UI.Xaml.Media.VisualTreeHelper.GetChild(node, i), name) is { } found)
            {
                return found;
            }
        }

        return null;
    }

    private void StartUiTestChannelIfEnabled()
        => uiTestChannel = UiTestChannel.TryStart(DispatcherQueue, ExecuteUiTestCommandAsync);

    private async Task<string> ExecuteUiTestCommandAsync(string command, IReadOnlyList<string> args)
    {
        switch (command.ToLowerInvariant())
        {
            case "state":
                return BuildUiTestStateDump();

            // ui.shot <png path> — the window as XAML draws it (RenderTargetBitmap), so a script can
            // look at a layout without taking the screen from whoever sits at the machine. Video in a
            // SwapChainPanel is not in it.
            // «Найти» in «Публичные релеи» without the mouse; answers with the status line it shows.
            // `relay.pin <node> <always|session>` / `relay.unpin`: what «Сидеть через него ▾» and
            // «Перестать» do, on a relay the last `relays.find` listed; answers with the banner.
            case "relay.pin":
            {
                Core.PublicRelay? found = args.Count < 2
                    ? null
                    : foundPublicRelays.FirstOrDefault(relay => relay.NodeId.StartsWith(args[0], StringComparison.OrdinalIgnoreCase));
                if (found is null)
                {
                    return "error: relay.pin <node-prefix from relays.find> <always|session>";
                }

                PinRelay(found, always: args[1] == "always");
                return $"ok {SettingsPinnedRelayInfoBar.Title} | {SettingsPinnedRelayInfoBar.Message}";
            }

            case "relay.unpin":
                UnpinRelay();
                return $"ok open={SettingsPinnedRelayInfoBar.IsOpen}";

            case "relay.pinned":
                LoadPinnedRelay();
                return SettingsPinnedRelayInfoBar.IsOpen
                    ? $"ok {SettingsPinnedRelayInfoBar.Severity} {SettingsPinnedRelayInfoBar.Title} | {SettingsPinnedRelayInfoBar.Message}"
                    : "ok none";

            case "relays.find":
                await FindPublicRelaysAsync();
                return $"ok {SettingsFindRelaysStatusTextBlock.Text} rows={SettingsPublicRelaysListPanel.Children.Count}";

            // `plan.do <contacts|omni_id|name|relay> <nostr|dht|ntfy> <with-above|alone|up|down|off|on-end|on-first>`:
            // what the step editor's «⋯» menu does, without the mouse; answers with the plan's summary.
            case "plan.do":
            {
                if (args.Count < 3)
                {
                    return "error: plan.do <publication> <kind> <action>";
                }

                string hostName = args[0] switch
                {
                    "contacts" => "SettingsContactsPlanHost",
                    "omni_id" => "SettingsOmniIdPlanHost",
                    "name" => "SettingsNamePlanHost",
                    _ => "SettingsRelayPlanHost",
                };
                return ResolveSectionElement<Border>(SettingsSectionHost, hostName).Child is Features.Shared.PublishPlanEditor editor
                    ? editor.Perform(args[1], args[2])
                    : "error: open the settings first (ui.settings plans)";
            }

            case "ui.shot":
            {
                if (args.Count == 0 || Content is not UIElement root)
                {
                    return "error: ui.shot <png path>";
                }

                var bitmap = new Microsoft.UI.Xaml.Media.Imaging.RenderTargetBitmap();
                await bitmap.RenderAsync(root);
                byte[] pixels = (await bitmap.GetPixelsAsync()).ToArray();
                using (var stream = new global::Windows.Storage.Streams.InMemoryRandomAccessStream())
                {
                    var encoder = await global::Windows.Graphics.Imaging.BitmapEncoder.CreateAsync(
                        global::Windows.Graphics.Imaging.BitmapEncoder.PngEncoderId, stream);
                    encoder.SetPixelData(
                        global::Windows.Graphics.Imaging.BitmapPixelFormat.Bgra8,
                        global::Windows.Graphics.Imaging.BitmapAlphaMode.Premultiplied,
                        (uint)bitmap.PixelWidth,
                        (uint)bitmap.PixelHeight,
                        96,
                        96,
                        pixels);
                    await encoder.FlushAsync();
                    byte[] png = new byte[stream.Size];
                    stream.Seek(0);
                    using var reader = new global::Windows.Storage.Streams.DataReader(stream);
                    await reader.LoadAsync((uint)stream.Size);
                    reader.ReadBytes(png);
                    File.WriteAllBytes(string.Join(' ', args), png);
                }

                return $"ok {bitmap.PixelWidth}x{bitmap.PixelHeight}";
            }

            // ui.bounds <x:Name> [minWidth] — where an element and its ancestors really sit (window
            // coordinates, actual and desired widths), and the descendants wider than minWidth: what
            // pushes a centred page sideways shows up as a desired width over the page's own.
            case "ui.bounds":
            {
                if (args.Count == 0 || Content is not FrameworkElement root)
                {
                    return "error: ui.bounds <name> [minWidth]";
                }

                FrameworkElement? target = FindNamedDescendant(root, args[0]);
                if (target is null)
                {
                    return "error: no element " + args[0];
                }

                double minWidth = args.Count > 1 && double.TryParse(args[1], System.Globalization.CultureInfo.InvariantCulture, out double parsed) ? parsed : double.MaxValue;
                var lines = new StringBuilder("ok\n");
                string Describe(FrameworkElement element)
                {
                    var origin = element.TransformToVisual(root).TransformPoint(new global::Windows.Foundation.Point(0, 0));
                    return FormattableString.Invariant(
                        $"{element.GetType().Name} '{element.Name}' x={origin.X:0} y={origin.Y:0} w={element.ActualWidth:0} h={element.ActualHeight:0} desired={element.DesiredSize.Width:0}");
                }

                for (DependencyObject? at = target; at is not null && at != root; at = Microsoft.UI.Xaml.Media.VisualTreeHelper.GetParent(at))
                {
                    if (at is FrameworkElement element)
                    {
                        lines.Append("up ").AppendLine(Describe(element));
                    }
                }

                void Walk(DependencyObject node, int depth)
                {
                    int count = Microsoft.UI.Xaml.Media.VisualTreeHelper.GetChildrenCount(node);
                    for (int i = 0; i < count; i++)
                    {
                        DependencyObject child = Microsoft.UI.Xaml.Media.VisualTreeHelper.GetChild(node, i);
                        if (child is FrameworkElement element && element.Visibility == Visibility.Visible && element.DesiredSize.Width >= minWidth)
                        {
                            lines.Append(new string(' ', depth * 2)).AppendLine(Describe(element));
                        }

                        Walk(child, depth + 1);
                    }
                }

                Walk(target, 1);
                return lines.ToString();
            }

            // ui.pane <open|close> — what the rail's toggle button does, so the open pane (item labels)
            // can be shot with ui.shot.
            case "ui.pane":
            {
                AuthenticatedNavigationView.IsPaneOpen = args.Count > 0 && args[0].Equals("open", StringComparison.OrdinalIgnoreCase);
                return $"ok pane={(AuthenticatedNavigationView.IsPaneOpen ? "open" : "closed")}";
            }

            // nav <chats|contacts|settings|diagnostics> [section] — the same switch a click on the
            // navigation rail makes (section = a Settings / Diagnostics page tag, e.g. `calls`), so a
            // script can shoot a page with ui.shot.
            case "nav":
            {
                if (args.Count < 1)
                {
                    return "error: nav <chats|contacts|settings|diagnostics> [section]";
                }

                AuthenticatedNavigationView.SelectedItem = args[0].ToLowerInvariant() switch
                {
                    "contacts" => ContactsNavigationItem,
                    "settings" => SettingsNavigationItem,
                    "diagnostics" => DiagnosticsNavigationItem,
                    _ => ChatsNavigationItem,
                };
                if (args.Count > 1)
                {
                    if (args[0].Equals("settings", StringComparison.OrdinalIgnoreCase))
                    {
                        SettingsSectionHost.ShowSection(args[1]);
                    }
                    else if (args[0].Equals("diagnostics", StringComparison.OrdinalIgnoreCase))
                    {
                        DiagnosticsSectionHost.ShowSection(args[1]);
                    }
                }

                await Task.Delay(1500);
                return $"ok\nsection={windowState.CurrentSection}";
            }

            // Settings → «Звонки» → «Микрофон и динамики»: what the pickers offer, what is chosen.
            // `audio.select mic|spk <index|default>` picks a row exactly as a click does (index = the
            // number `audio.devices` prints), so the choice goes through the same handler and file.
            case "audio.devices":
            {
                NativeWebRtcMediaEngine.AudioDeviceList devices =
                    await Task.Run(NativeWebRtcMediaEngine.ListAudioDevices);
                var lines = new StringBuilder("ok\n");
                for (int i = 0; i < devices.Microphones.Count; i++)
                {
                    var device = devices.Microphones[i];
                    lines.AppendLine($"mic\t{i}\t{device.Name}\tdefault={device.IsDefault}\tchosen={string.Equals(device.Id, callVideoService.PreferredMicrophoneId, StringComparison.OrdinalIgnoreCase)}");
                }

                for (int i = 0; i < devices.Speakers.Count; i++)
                {
                    var device = devices.Speakers[i];
                    lines.AppendLine($"spk\t{i}\t{device.Name}\tdefault={device.IsDefault}\tchosen={string.Equals(device.Id, callVideoService.PreferredSpeakerId, StringComparison.OrdinalIgnoreCase)}");
                }

                lines.Append($"preferred mic={callVideoService.PreferredMicrophoneId ?? "default"} spk={callVideoService.PreferredSpeakerId ?? "default"}");
                return lines.ToString();
            }

            case "audio.select":
            {
                if (args.Count < 2 || args[0] is not ("mic" or "spk"))
                {
                    return "error: audio.select mic|spk <index|default>";
                }

                NativeWebRtcMediaEngine.AudioDeviceList devices =
                    await Task.Run(NativeWebRtcMediaEngine.ListAudioDevices);
                IReadOnlyList<NativeWebRtcMediaEngine.AudioDeviceOption> list =
                    args[0] == "mic" ? devices.Microphones : devices.Speakers;
                string? id = null;
                if (args[1] != "default")
                {
                    if (!int.TryParse(args[1], out int index) || index < 0 || index >= list.Count)
                    {
                        return $"error: no device {args[1]} (have {list.Count})";
                    }

                    id = list[index].Id;
                }

                Microsoft.UI.Xaml.Controls.ComboBox combo = args[0] == "mic" ? SettingsMicrophoneComboBox : SettingsSpeakerComboBox;
                RefreshAudioDevicePickers();
                await Task.Delay(1000);
                object? row = combo.Items.FirstOrDefault(item =>
                    item is AudioDevicePickerOption option &&
                    string.Equals(option.DeviceId, id, StringComparison.OrdinalIgnoreCase));
                if (row is null)
                {
                    return "error: the picker has no such row";
                }

                combo.SelectedItem = row;
                return $"ok\npreferred mic={callVideoService.PreferredMicrophoneId ?? "default"} spk={callVideoService.PreferredSpeakerId ?? "default"}";
            }

            case "chats":
                return BuildUiTestChatList();

            // «Мои устройства» (Settings): тот же вызов, что рисует карточку. `device.revoke <id>`
            // зовёт тот же мостовой вызов, что кнопка «Отозвать», минуя только диалог подтверждения
            // (за ним некому нажать).
            case "devices":
                return "ok\n" + string.Join(
                    "\n",
                    rustBridge.ListOwnDevices().Select(device =>
                        $"device\t{device.DeviceId}\t{device.DeviceLabel}\tthis={device.IsThisDevice}\trevoked={device.Revoked}\tonline={device.Online}"));

            // «Ключ аккаунта украден»: перенос на новый рабочий ключ, оставляя перечисленные устройства.
            case "account.move":
            {
                AccountKeyMoveSummary summary = rustBridge.MoveAccountToNewWorkingKey(args.ToList(), null, null);
                return $"ok\nversion={summary.NewWorkingKeyVersion}\nkept={summary.KeptDevices}";
            }

            // Couriers (docs/decisions/courier-metadata-and-quota.md, step 6): the list as the card
            // shows it, and a mode change through the card's own path (the row's mode box handler).
            case "dht.status":
                return rustBridge.GetDhtRendezvousStatus() is { } dht
                    ? $"ok\nenabled={dht.Enabled} running={dht.Running} nodes={dht.NodeCount} empty_for_ms={dht.EmptyForMs} way_in={dht.WayIn ?? "-"} friend_proxy={dht.FriendProxy}"
                    : "error no status";

            case "couriers":
                return "ok\n" + string.Join(
                    "\n",
                    rustBridge.ListRendezvousProviders().Select(courier =>
                        $"courier\t{courier.Id}\t{courier.Url}\tmode={courier.Mode}\tstatus={courier.Status}\tpooled={courier.Pooled}"));

            case "courier.mode":
                if (args.Count < 2 || !long.TryParse(args[0], out long courierId))
                {
                    return "error: courier.mode <id> <publish_read|read|off>";
                }

                SetRendezvousProviderMode(courierId, args[1]);
                return "ok\n" + SettingsProvidersStatusTextBlock.Text;

            // The profile as the shell shows it (header and editor), and a rename through the
            // editor's own Save button — another device of ours should take it
            // (multi-device-equal-nodes.md, «Синхронизация профиля»).
            case "profile":
                return $"ok\nshown={windowState.Onboarding.LocalAccount?.Descriptor.DisplayName}\neditor={SettingsPublicDisplayNameTextBox.Text}\navatar={windowState.Onboarding.LocalAccount?.PublicAvatarHashHex}";

            case "profile.name":
                if (args.Count < 1)
                {
                    return "error: profile.name <display name>";
                }

                SettingsPublicDisplayNameTextBox.Text = string.Join(' ', args);
                OnSettingsSavePublicProfileClicked(this, new RoutedEventArgs());
                return await SettleUiTestCommandAsync();

            // «Сохранить профиль» as it stands, without touching the editor.
            case "profile.save":
                OnSettingsSavePublicProfileClicked(this, new RoutedEventArgs());
                return await SettleUiTestCommandAsync();

            // The password of «Вход на этом компьютере»: the same boxes and buttons as in Settings,
            // and the unlock screen's. Each answers with the status line the person reads.
            case "password.set":
            case "password.change":
            case "password.remove":
            {
                SettingsNewPasswordBox.Password = command == "password.set" ? string.Join(' ', args) : string.Empty;
                SettingsCurrentPasswordBox.Password = command != "password.set" && args.Count > 0 ? args[0] : string.Empty;
                SettingsChangePasswordBox.Password = command == "password.change" && args.Count > 1 ? args[1] : string.Empty;
                switch (command)
                {
                    case "password.set":
                        OnSettingsSetPasswordClicked(this, new RoutedEventArgs());
                        break;
                    case "password.change":
                        OnSettingsChangePasswordClicked(this, new RoutedEventArgs());
                        break;
                    default:
                        OnSettingsRemovePasswordClicked(this, new RoutedEventArgs());
                        break;
                }

                await Task.Delay(1500);
                return $"ok\nstatus={SettingsStatusTextBlock.Text}";
            }

            // «Выйти» with its dialog confirmed: the preview, its token, the same core call and the
            // same return to the profile list as OnSettingsSignOutClicked.
            case "profile.signout":
            {
                string? profileId = GetCurrentProfileId();
                if (string.IsNullOrWhiteSpace(profileId))
                {
                    return "error: no active profile";
                }

                DangerousActionPreview? preview = await Task.Run(() => rustBridge.PreviewSignOutProfile(profileId));
                if (preview is null)
                {
                    return "error: no sign-out preview";
                }

                await Task.Run(() => rustBridge.ConfirmSignOutProfile(profileId, preview.ConfirmationToken));
                ReturnToInitialShellAfterProfileExit("Вы вышли из профиля.");
                return "ok\n" + await SettleUiTestCommandAsync();
            }

            // «Открыть данные фразой» (a data folder from another machine): whether it is open and what
            // it says; `boot.unlock <words…>` types the phrase and presses «Открыть».
            case "boot.lock":
                return $"ok\nboot_lock={(bootLockDialog is null ? "none" : "open")}\nerror={bootLockErrorText ?? ""}\nboot_status={BootStatusTextBlock.Text}";

            case "boot.unlock":
            {
                if (bootLockDialog is null || bootLockPhraseBox is null)
                {
                    return "error: the «Открыть данные фразой» dialog is not open";
                }

                bootLockPhraseBox.Text = string.Join(' ', args);
                bootLockForcedResult = ContentDialogResult.Primary;
                bootLockDialog.Hide();
                await Task.Delay(3000);
                return $"ok\nboot_lock={(bootLockDialog is null ? "none" : "open")}\nerror={bootLockErrorText ?? ""}\n" + await SettleUiTestCommandAsync();
            }

            case "unlock":
                UnlockPasswordBox.Password = string.Join(' ', args);
                OnUnlockWithPasswordClicked(this, new RoutedEventArgs());
                await Task.Delay(2500);
                return $"ok\nstatus={OnboardingStatusTextBlock.Text}\n" + await SettleUiTestCommandAsync();

            case "devices.ack":
                OnAcknowledgeOwnDevicesClicked(this, new RoutedEventArgs());
                return "ok";

            case "device.revoke":
                if (args.Count < 1)
                {
                    return "error: device.revoke <device_id>";
                }

                rustBridge.RevokeOwnDevice(args[0], args.Count > 1 ? args[1] : null);
                return "ok";

            // Онбординг и пейринг. До 2026-08-14 канал умел только звонки, и поднять СВЕЖИЙ стенд
            // без человека было нельзя — а именно свежий и нужен, когда проверяешь пересборку
            // сессии на чистой паре (docs/bugs/session-wedged-by-counter-gap.md). Команды зовут
            // ровно те же мостовые вызовы, что и кнопки в интерфейсе: канал не получает никакого
            // обходного пути мимо продуктового кода.
            case "profile.create":
            {
                if (args.Count == 0)
                {
                    return "error: profile.create needs a display name";
                }

                // Заполняем те же поля и жмём ту же кнопку, что и человек: весь пост-обработчик
                // (профиль, устройство, переход из онбординга) отрабатывает продуктовым кодом.
                CreateDisplayNameTextBox.Text = args[0];
                CreateDeviceLabelTextBox.Text = args.Count > 1 ? args[1] : "TestDevice";
                OnCreateAccountClicked(this, new RoutedEventArgs());
                // Создание аккаунта уводит на шаг «сохраните фразу восстановления», и без явного
                // продолжения оболочка остаётся в онбординге. Человек тут жмёт «Продолжить»;
                // скрипту нужно то же самое, иначе профиль есть, а работать нечем.
                await Task.Delay(1500);
                // The phrase the person writes down — a test profile's only, for the restore runs.
                string createdPhrase = windowState.Onboarding.PendingRecoveryPhraseV2?.Mnemonic ?? string.Empty;
                OnRecoveryContinueClicked(this, new RoutedEventArgs());
                return $"phrase={createdPhrase}\n" + await SettleUiTestCommandAsync();
            }

            // «Восстановить по фразе» (docs/decisions/account-recovery-from-friends.md): the fields
            // and the button of the restore screen.
            case "profile.restore":
            {
                if (args.Count < 3)
                {
                    return "error: profile.restore <name> <device> <phrase words…>";
                }

                RestoreDisplayNameTextBox.Text = args[0];
                RestoreDeviceLabelTextBox.Text = args[1];
                RestoreRecoveryPhraseTextBox.Text = string.Join(" ", args.Skip(2));
                OnRestoreAccountClicked(this, new RoutedEventArgs());
                await Task.Delay(3000);
                return $"status={OnboardingStatusTextBlock.Text}\n" + await SettleUiTestCommandAsync();
            }

            // The backup file (Г): «Сохранить резервную копию» without the folder picker, and the
            // import screen's file + phrase + «Восстановить».
            case "backup.save":
            {
                if (args.Count < 1)
                {
                    return "error: backup.save <path.omnibackup>";
                }

                await Task.Run(() => rustBridge.ExportCurrentDatabaseBackupBundle(args[0]));
                AccountExportManifestV1? saved = await Task.Run(() => rustBridge.GetCurrentAccountExportManifest());
                return $"ok\nbytes={new System.IO.FileInfo(args[0]).Length}\nreminder={BackupReminder.Describe(saved?.LastBackupAtUnixMs, DateTimeOffset.Now)}";
            }

            case "backup.restore":
            {
                if (args.Count < 2)
                {
                    return "error: backup.restore <path> <phrase words…>";
                }

                System.IO.FileInfo backupFile = new(args[0]);
                windowState.Onboarding.PendingImportFilePath = backupFile.FullName;
                windowState.Onboarding.PendingImportFileName = backupFile.Name;
                backupImportSession.SelectFile(backupFile.FullName);
                ImportAccountMnemonicTextBox.Text = string.Join(" ", args.Skip(1));
                OnImportAccountClicked(this, new RoutedEventArgs());
                await Task.Delay(5000);
                return $"status={ImportAccountStatusTextBlock.Text}\n" + await SettleUiTestCommandAsync();
            }

            // The chat header's subtitle (history check marks: «история не сходится» …).
            case "chat.header":
            {
                if (args.Count < 1)
                {
                    return "error: chat.header <dialog_key>";
                }

                ChatHeaderContactSummary header = rustBridge.GetChatHeaderContactSummary(args[0]);
                return $"ok\ntitle={header.Title}\nsubtitle={header.Subtitle}";
            }

            // Make a saved profile the active one, as «Открыть» in the profile chooser does. The mesh of
            // the current one stops; restart the app to come up in the reopened profile.
            case "profile.open":
            {
                if (args.Count < 1)
                {
                    return "error: profile.open <profile_id>";
                }

                rustBridge.StopMesh();
                LocalProfileSummary reopened = await Task.Run(() => rustBridge.ReopenLocalProfile(args[0]));
                return $"ok\nactive={rustBridge.GetActiveProfileId()}\nname={reopened.DisplayName}\nrestart the app to load it";
            }

            // The carrier of our hops (Settings → «Для опытных» → «Несущая соединения»), applied at once
            // by the core to the running mesh, as from the picker.
            case "carrier":
                if (args.Count < 1 || args[0] is not ("auto" or "tcp" or "quic"))
                {
                    return $"ok\ncarrier={rustBridge.GetCarrierPolicy()}";
                }

                string carrier = args[0];
                await Task.Run(() => rustBridge.SetCarrierPolicy(carrier));
                RefreshCarrierPolicySelection();
                return $"ok\ncarrier={rustBridge.GetCarrierPolicy()}";

            case "mesh.start":
                rustBridge.StartMesh();
                return await SettleUiTestCommandAsync();

            // Каким путём идёт связь, а не только «дошла ли». Без этого двухмашинный гейт
            // недоказуем: звонок через релей и звонок напрямую через настоящий NAT дают в
            // `state` одинаковую зелень, а на обеих тестовых машинах ещё и поднят Radmin
            // (26.0.0.0/8) ради управляющего канала — линк, уехавший в него, выглядел бы
            // так же. Отдаём ровно то, что уже отдаёт ядро (`omni_mesh_connection_state`),
            // ничего не выводя самостоятельно: Rust = истина, канал только разворачивает DTO.
            case "mesh.state":
                return BuildUiTestMeshStateDump();

            case "pair.invite":
                return rustBridge.GenerateMeshPairingInvite();

            case "pair.accept":
            {
                if (args.Count == 0)
                {
                    return "error: pair.accept needs an invite token";
                }

                rustBridge.RegisterMeshPairingInvite(args[0]);
                return await SettleUiTestCommandAsync();
            }

            case "contacts.approve-all":
            {
                int approved = 0;
                // Берём тот же снимок, что и экран контактов. `ListContacts()` тут не годится: он
                // не используется больше нигде и на пустом списке падает разбором JSON — см.
                // docs/bugs/list-contacts-bridge-call-is-broken.md.
                foreach (ContactSnapshot contact in rustBridge.GetContactsScreenSnapshot().Contacts)
                {
                    if (!contact.RelationshipState.Equals("pending", StringComparison.OrdinalIgnoreCase))
                    {
                        continue;
                    }

                    rustBridge.ApproveContact(contact.ContactId);
                    approved++;
                }

                return $"ok approved={approved}" + Environment.NewLine
                    + await SettleUiTestCommandAsync();
            }

            // Contact sync (multi-device-equal-nodes.md, «Синхронизация контактов»): the list as the
            // contacts screen has it, and the same buttons a person presses, on a contact picked by
            // name, contact id or account id.
            case "contacts":
                return "ok\n" + string.Join(
                    "\n",
                    rustBridge.GetContactsScreenSnapshot().Contacts.Select(contact =>
                        $"contact\t{contact.ContactId}\t{contact.Petname}\t{contact.RelationshipState}\tblocked={contact.IsBlocked}\taccount={contact.RemoteAccountId}"));

            case "contact.block":
            case "contact.remove":
            case "contact.approve":
            {
                string? wanted = args.Count > 0 ? string.Join(' ', args) : null;
                ContactSnapshot? target = rustBridge.GetContactsScreenSnapshot().Contacts.FirstOrDefault(contact =>
                    contact.ContactId == wanted || contact.Petname == wanted || contact.RemoteAccountId == wanted);
                if (target is null)
                {
                    return $"error: {command} needs a known contact (name, contact id or account id)";
                }

                windowState.SelectedContactId = target.ContactId;
                switch (command)
                {
                    case "contact.block":
                        OnBlockContactClicked(this, new RoutedEventArgs());
                        break;
                    // «Забыть» past its confirmation dialog — the only removal the UI has (the old
                    // «помечен как удалённый» handler was reachable from this channel alone).
                    case "contact.remove":
                        ContactSnapshot forgotten = rustBridge.ForgetContact(target.ContactId);
                        windowState.SelectedContactId = null;
                        RefreshContactsList(true);
                        RefreshChatsList(true);
                        return $"ok\nforgot={forgotten.Petname}";
                    default:
                        OnApproveContactClicked(this, new RoutedEventArgs());
                        break;
                }

                return $"ok\nstatus={ContactManagementStatusTextBlock.Text}";
            }

            case "chat.open":
                if (args.Count == 0)
                {
                    return "error: chat.open needs a dialog key";
                }

                ShowChatsSectionForDialog(args[0], contactId: null);
                return "ok";

            // A tap on the open chat's newest photo: the full-window viewer (`viewer.close` closes it).
            case "viewer.open":
            {
                ChatMessageRecord? photo = currentChatMessagesById.Values
                    .Where(m => m.Attachment?.Presentation is { IsPhoto: true })
                    .OrderByDescending(m => m.CreatedAtUnixMs)
                    .FirstOrDefault();
                if (photo?.Attachment?.LocalCopyPath is not { } path)
                {
                    return "error: no downloaded photo in the open chat";
                }

                OpenImageViewer(path, "test", photo.Attachment.FileName);
                // The picture comes from the core's plaintext (the photo folder is encrypted at rest).
                await Task.Delay(1500);
                return $"ok {path}\npicture={ImageViewerHost.HasPicture.ToString().ToLowerInvariant()}";
            }

            // What the shell gets of a media file (a photo, an avatar) through the core — the way every
            // picture is read, Omni's storage being encrypted at rest: its size and first bytes.
            case "media.read":
            {
                if (args.Count < 1)
                {
                    return "error: media.read <path>";
                }

                byte[]? bytes = await Task.Run(() => MediaFiles.ReadAllBytes(string.Join(' ', args)));
                return bytes is null
                    ? "ok\nread=false"
                    : $"ok\nread=true\nbytes={bytes.Length}\nhead={Convert.ToHexString(bytes.AsSpan(0, Math.Min(4, bytes.Length)))}";
            }

            case "viewer.close":
                ImageViewerHost.Close();
                return "ok";

            // A tap on the open chat's header: the contact's page.
            case "chat.profile":
                OpenCurrentChatContactProfile();
                return "ok";

            // Отправка и разбор чата. Нужны, чтобы проверять расхождение счётчиков сессии живьём:
            // разрыв растёт по числу РАЗНЫХ сообщений (docs/bugs/session-wedged-by-counter-gap.md,
            // часть 1), и никаким другим способом его на исправной сети не получить.
            case "chat.send":
            {
                if (args.Count == 0)
                {
                    return "error: chat.send needs a message body";
                }

                MessageInputTextBox.Text = string.Join(' ', args);
                OnSendClicked(this, new RoutedEventArgs());
                return await SettleUiTestCommandAsync();
            }

            // Файл: тот же черновик и та же кнопка «Отправить», что у человека, только без окна
            // выбора файла и вопроса о режиме (сквозное шифрование). Нужно для живой проверки
            // копий вложений своим устройствам (docs/decisions/multi-device-equal-nodes.md).
            case "chat.file":
            case "chat.photo":
            {
                if (args.Count == 0 || !File.Exists(args[0]))
                {
                    return "error: chat.file needs an existing file path";
                }

                string stagedPath = await StageAttachmentSourceAsync(args[0]);
                SetPendingAttachmentDraft(stagedPath, Path.GetFileName(args[0]));
                // chat.file sends the file itself (the exe deploy rides it); chat.photo a photo.
                pendingAttachmentAsPhoto = string.Equals(command, "chat.photo", StringComparison.OrdinalIgnoreCase);
                MessageInputTextBox.Text = string.Join(' ', args.Skip(1));
                OnSendClicked(this, new RoutedEventArgs());
                return await SettleUiTestCommandAsync();
            }

            case "chat.download":
            {
                if (args.Count < 2)
                {
                    return "error: chat.download needs a message id and a save path";
                }

                string? downloadDialog = GetResolvedCurrentDialogKey();
                ChatMessageRecord? message = string.IsNullOrWhiteSpace(downloadDialog)
                    ? null
                    : rustBridge.ListMessages(downloadDialog).Messages.FirstOrDefault(entry => entry.Id == args[0]);
                if (message is null)
                {
                    return "error: chat.download: no such message in the open chat";
                }

                await StartAttachmentDownloadAsync(message.Id, args[1]);
                return await SettleUiTestCommandAsync();
            }

            case "chat.log":
            {
                string? logDialog = args.Count > 0 ? args[0] : GetResolvedCurrentDialogKey();
                if (string.IsNullOrWhiteSpace(logDialog))
                {
                    return "error: chat.log needs an open dialog or an explicit dialog key";
                }

                return BuildUiTestMessageLog(logDialog);
            }

            case "call.place":
                OnStartCurrentChatCallClicked(this, new RoutedEventArgs());
                return await SettleUiTestCommandAsync();

            case "call.accept":
                await AcceptIncomingCallAsync(micMuted: false);
                return await SettleUiTestCommandAsync();

            // "Без микрофона" — the answer that opens the microphone already muted.
            case "call.accept-muted":
                await AcceptIncomingCallAsync(micMuted: true);
                return await SettleUiTestCommandAsync();

            case "call.decline":
                OnDeclineIncomingCallClicked(this, new RoutedEventArgs());
                return await SettleUiTestCommandAsync();

            // The chevron on the call screen, and the strip's "Вернуться к звонку".
            case "call.fold":
                OnMinimizeCallClicked(this, new RoutedEventArgs());
                return $"ok strip={ActiveCallBar.Visibility} screen={CallOverlayRoot.Visibility}";

            case "call.return":
                OnReturnToCallClicked(this, new RoutedEventArgs());
                return $"ok strip={ActiveCallBar.Visibility} screen={CallOverlayRoot.Visibility}";

            case "call.hangup":
                OnHangUpCurrentCallClicked(this, new RoutedEventArgs());
                return await SettleUiTestCommandAsync();

            case "camera.on":
                await callVideoService.StartLocalCameraAsync();
                AnnounceGroupMediaStateForUiTest();
                return await SettleUiTestCommandAsync();

            case "camera.off":
                await callVideoService.StopLocalCameraAsync();
                AnnounceGroupMediaStateForUiTest();
                return await SettleUiTestCommandAsync();

            // Смена качества посреди звонка — тот самый путь, на котором согласование сносилось
            // целиком и вместе с ним закрывались ворота для входящей медиа
            // (docs/bugs/one-button-press-killed-the-call.md). `camera.on` его НЕ проходит:
            // он зовёт сервис напрямую, минуя настройки, поэтому дефект гейт и не видел.
            // The codec ceiling, exactly as the Settings list sets it (mid-call too: a new passport,
            // a new selection, both directions rebuilt — per-pair-video-codec-selection.md §6).
            case "codec.ceiling":
            {
                CallVideoCodecPreference? wanted = args.Count == 0 ? null : args[0].ToLowerInvariant() switch
                {
                    "auto" => CallVideoCodecPreference.Auto,
                    "h265" => CallVideoCodecPreference.UpToH265,
                    "h264" => CallVideoCodecPreference.H264,
                    _ => null,
                };
                if (wanted is null)
                {
                    return "error: codec.ceiling needs auto|h265|h264";
                }

                CallVideoCodecComboBox.SelectedItem = callVideoCodecPreferenceOptions.First(o => o.Preference == wanted);
                return await SettleUiTestCommandAsync();
            }

            // The bitrate level, exactly as the Settings list sets it — mid-call too.
            case "bitrate.level":
            {
                CallVideoBitratePreference? wanted = args.Count == 0 ? null : args[0].ToLowerInvariant() switch
                {
                    "economy" => CallVideoBitratePreference.Economy,
                    "auto" => CallVideoBitratePreference.Auto,
                    "max" => CallVideoBitratePreference.Max,
                    _ => null,
                };
                if (wanted is null)
                {
                    return "error: bitrate.level needs economy|auto|max";
                }

                CallVideoBitrateComboBox.SelectedItem = callVideoBitratePreferenceOptions.First(o => o.Preference == wanted);
                return await SettleUiTestCommandAsync();
            }

            // The codec of one direction "breaks" (§6): out of the passport for the rest of the call,
            // re-announced, the pair moves to the next codec.
            case "codec.break":
            {
                if (args.Count == 0 || args[0] is not ("encode" or "decode"))
                {
                    return "error: codec.break needs encode|decode";
                }

                return await callVideoService.SimulateVideoCodecFailureAsync(encoder: args[0] == "encode");
            }

            // Find by Omni-ID / by name (docs/decisions/find-by-name.md), without touching the window.
            case "discover.mode":
                rustBridge.SetDiscoverabilityMode(args.Count > 0 && args[0] == "public" ? "public" : "off");
                RefreshDiscoverabilitySettings();
                return $"ok mode={rustBridge.GetDiscoverabilityMode()}";

            case "omni.id":
                return $"ok {rustBridge.GetMyOmniId()}";

            // The «Найти человека» field and its button, with an Omni-ID: what the status line says after.
            case "find.id":
                if (args.Count < 1)
                {
                    return "error: find.id <omni:…>";
                }

                AddByOmniIdTextBox.Text = args[0];
                OnAddByOmniIdClicked(this, new RoutedEventArgs());
                await Task.Delay(300);
                for (int waited = 0; !AddByOmniIdButton.IsEnabled && waited < 120; waited++)
                {
                    await Task.Delay(500);
                }

                return $"ok status={AddByOmniIdStatusTextBlock.Text}";

            // «Выбрать фото» without the file dialog, then «Сохранить профиль».
            case "profile.avatar":
                if (args.Count < 1 || !System.IO.File.Exists(args[0]))
                {
                    return "error: profile.avatar <existing image path>";
                }

                DeletePendingPublicAvatarSourceFile();
                pendingPublicAvatarSourcePath = await StagePublicAvatarSourceAsync(args[0]);
                pendingPublicAvatarDisplayName = System.IO.Path.GetFileName(args[0]);
                pendingPublicAvatarClearRequested = false;
                OnSettingsSavePublicProfileClicked(this, new RoutedEventArgs());
                return await SettleUiTestCommandAsync();

            case "name.findable":
                rustBridge.SetNameFindable(args.Count > 0 && args[0] == "on");
                RefreshDiscoverabilitySettings();
                return $"ok name_findable={rustBridge.GetNameFindable().ToString().ToLowerInvariant()}";

            case "name.search":
            {
                NameSearchResult found = await Task.Run(() => rustBridge.SearchPeopleByName(string.Join(' ', args)));
                StringBuilder lines = new($"ok status={found.Status} people={found.People.Count}");
                foreach (FoundByName person in found.People)
                {
                    lines.Append($"{Environment.NewLine}person	{person.OmniId}	{person.DisplayName}	whole={person.WholeName}	photo={(person.PhotoJpegB64 is null ? 0 : person.PhotoJpegB64.Length)}	contact={person.IsContact}	self={person.IsSelf}");
                }
                return lines.ToString();
            }

            case "camera.quality":
            {
                if (args.Count == 0 ||
                    !TryParseUiTestResolution(args[0], out CallVideoResolutionOption requested))
                {
                    return "error: camera.quality needs a resolution like 640x360";
                }

                // Ровно то, что делает пункт меню качества на кнопке камеры.
                if (!await TryApplyResolutionAsync(requested))
                {
                    return $"error: camera.quality rejected {args[0]}";
                }

                return await SettleUiTestCommandAsync();
            }

            // Смена контроллера перегрузки БЕЗ перезапуска приложения и БЕЗ перезапуска меша —
            // перезапуск это новый процесс, новый пайплайн камеры и минуты между прогонами, за
            // которые канал успевает уехать вдвое (живьём 2026-08-31: RTT пути 49 -> 105 мс между
            // соседними прогонами обесценил всю пару). Зовёт ровно тот же путь, что переключатель
            // в настройках: пересборка конфигов интерфейсов и закрытие соединений, которые едут на
            // прежнем контроллере. Меш при этом жив — дискавери, анонсы и проброс порта целы.
            case "cc.set":
            {
                if (args.Count == 0)
                {
                    return "error: cc.set needs auto|bbr|cubic|newreno";
                }

                string requested = args[0].Trim().ToLowerInvariant();
                if (requested != CongestionControlPreference.Auto &&
                    requested != CongestionControlPreference.Bbr &&
                    requested != CongestionControlPreference.Cubic &&
                    requested != CongestionControlPreference.NewReno)
                {
                    return $"error: cc.set rejected {args[0]}";
                }

                SavePersistedCongestionMode(requested);
                ApplyCongestionModeToCore(requested);
                ApplyCongestionChangeToLiveLinks(requested);
                return $"ok\ncc_requested={requested}";
            }

            // Что действительно применится к следующему соединению — с учётом OMNI_QUIC_CC и
            // стоимости канала. Без этого прогон не может доказать, ЧЕМ он ехал.
            case "cc.get":
                return $"ok\ncc_selected={LoadPersistedCongestionMode()}\n" +
                       $"cc_effective={rustBridge.ReadEffectiveCongestionControl()}";

            case "screen.on":
            {
                // No system picker: an automated pass has no human to click it. Primary
                // monitor, always — the picker stays the path for interactive sharing.
                // `screen.on 1080` — the size from the menu by its height; without it, the default.
                var monitor = ScreenShareCaptureSource.CreateForPrimaryMonitor();
                CallVideoResolutionOption? size = null;
                if (args.Count > 0 && int.TryParse(args[0], out int height))
                {
                    size = CallVideoPreferences.BuildResolutionOptions(monitor.Size.Width, monitor.Size.Height)
                        .Where(option => option.Height == height)
                        .Select(option => (CallVideoResolutionOption?)option)
                        .FirstOrDefault();
                    if (size is null)
                    {
                        return $"error: the screen has no {height}p option";
                    }
                }

                await callVideoService.StartScreenShareAsync(monitor, size);
                // As the group button does: a group call must hear that we share.
                AnnounceGroupMediaStateForUiTest();
                return await SettleUiTestCommandAsync();
            }

            case "screen.off":
                await callVideoService.StopScreenShareAsync();
                AnnounceGroupMediaStateForUiTest();
                return await SettleUiTestCommandAsync();

            case "pattern.on":
                // A synthetic, deterministic source that always wants more than the path carries —
                // for unattended rate/latency measurements, where a real screen may be off (no
                // frames at all) and a camera in the dark gives one or two a second.
                // Stopped by "screen.off", like any share.
                await callVideoService.StartSyntheticPatternShareAsync();
                AnnounceGroupMediaStateForUiTest();
                return await SettleUiTestCommandAsync();

            case "mic.mute":
            case "mic.unmute":
            {
                // Both used to press the toggle: "mic.mute" on a muted mic turned it ON (live
                // 2026-10-10, a group call ran with the laptop's real mic open). Now each names the
                // state it leaves, pressing the button of the call that is on only when it differs.
                bool wantMuted = command.Equals("mic.mute", StringComparison.OrdinalIgnoreCase);
                bool inGroupCall = currentCallInboxSnapshot?.CurrentCall is null &&
                                   (callVideoService.ActiveDialogKey?.StartsWith("group:", StringComparison.Ordinal) ?? false);
                // A 1:1 call's truth is its summary (what the button toggles). The media service's
                // flag is reset only when media comes up, so before that it still holds the LAST
                // call's: "mic.mute" right after call.place read "already muted" from a call that
                // had ended muted, pressed nothing, and the new call went out with the mic open
                // (live 2026-10-14).
                bool currentlyMuted = inGroupCall
                    ? callVideoService.IsLocalAudioMuted
                    : GetActionableCurrentChatCall()?.LocalMicMuted ?? callVideoService.IsLocalAudioMuted;
                if (currentlyMuted != wantMuted)
                {
                    if (inGroupCall)
                    {
                        GroupConversation.ToggleMuteForUiTest();
                    }
                    else
                    {
                        OnToggleCurrentCallMuteClicked(this, new RoutedEventArgs());
                    }
                }

                return await SettleUiTestCommandAsync();
            }

            default:
                return TryExecuteScreenControlUiTestCommand(command.ToLowerInvariant(), args)
                    ?? TryExecuteRemoteAccessUiTestCommand(command.ToLowerInvariant(), args)
                    ?? await TryExecuteGroupUiTestCommandAsync(command.ToLowerInvariant(), args)
                    ?? $"error: unknown command '{command}'";
        }
    }

    /// <summary>
    /// Let the fire-and-forget handler get somewhere before we answer, then report the
    /// state we ended up in. A script that reads `ok` and immediately asserts would
    /// otherwise be racing the very handler it just triggered.
    /// </summary>
    private async Task<string> SettleUiTestCommandAsync()
    {
        await Task.Delay(400);
        RefreshCallInbox();
        return "ok" + Environment.NewLine + BuildUiTestStateDump();
    }

    private string BuildUiTestChatList()
    {
        StringBuilder builder = new("ok");
        builder.AppendLine();
        foreach (RustBridge.ChatThreadProjection chat in rustBridge.ListChatThreadProjections())
        {
            builder.AppendLine(
                $"chat\t{chat.Contact.PrimaryDialogKey ?? "none"}\t{chat.Contact.Petname}\t{chat.Contact.RelationshipState}\tpublic={chat.Contact.PublicDisplayName}\tunread={chat.UnreadCount}");
        }

        // What the rows and the rail show (after the open chat was read), not only what the core counts.
        foreach (Features.Chats.ChatThreadListItem row in chatThreadItems)
        {
            builder.AppendLine($"row\t{row.DialogKey}\t{row.Title}\tpill={(row.UnreadBadgeVisibility == Visibility.Visible ? row.UnreadBadgeText : "-")}");
        }

        builder.AppendLine($"rail\t{(ChatsNavigationItem.InfoBadge as Microsoft.UI.Xaml.Controls.InfoBadge)?.Value.ToString(System.Globalization.CultureInfo.InvariantCulture) ?? "-"}");
        return builder.ToString().TrimEnd();
    }

    /// <summary>
    /// Everything an automated pass needs to assert on, as `key=value` lines. Deliberately
    /// includes the things past live tests had to be reverse-engineered from a trace: is a
    /// call live, is the microphone open, which video source is running, what got
    /// negotiated, and whether the camera button is actually clickable.
    /// </summary>
    /// <summary>
    /// Сводка по диалогу плюс сами сообщения. Сводка — то, на чём держится проверка расклинивания:
    /// «сколько исходящих ещё в очереди» и «сколько входящих дошло» отвечают на вопрос напрямую,
    /// без разбора трассы.
    /// </summary>
    private string BuildUiTestMessageLog(string dialogKey)
    {
        IReadOnlyList<ChatMessageRecord> messages = rustBridge.ListMessages(dialogKey).Messages;
        int outgoing = 0;
        int outgoingDelivered = 0;
        int outgoingReceipted = 0;
        int incoming = 0;
        foreach (ChatMessageRecord message in messages)
        {
            if (message.Direction.Equals("outgoing", StringComparison.OrdinalIgnoreCase))
            {
                outgoing++;
                if (message.Status.Equals("delivered", StringComparison.OrdinalIgnoreCase))
                {
                    outgoingDelivered++;
                }

                // Статус и расписка — РАЗНЫЕ факты, и они расходятся: у событий истории звонков
                // статус «delivered» стоит без всякой расписки от той стороны. Считаем оба, иначе
                // проверка доставки меряет не то, что думает.
                if (message.RemoteDeliveredAtUnixMs is not null)
                {
                    outgoingReceipted++;
                }
            }
            else
            {
                incoming++;
            }
        }

        StringBuilder builder = new("ok");
        builder.AppendLine();
        builder.AppendLine($"dialog={dialogKey}");
        builder.AppendLine($"outgoing={outgoing}");
        builder.AppendLine($"outgoing_delivered={outgoingDelivered}");
        builder.AppendLine($"outgoing_receipted={outgoingReceipted}");
        builder.AppendLine($"incoming={incoming}");
        foreach (ChatMessageRecord message in messages)
        {
            builder.AppendLine(
                $"message\t{message.Direction}\t{message.Status}\t{(message.RemoteDeliveredAtUnixMs is null ? "-" : "delivered")}\t{(message.ToldByContact ? "told" : "-")}\t{message.Body.Replace('\t', ' ').Replace('\n', ' ')}");
            if (message.Attachment is { } attachment)
            {
                builder.AppendLine(
                    $"attachment\t{message.Id}\t{attachment.State}\t{attachment.BytesTransferred}/{attachment.FileSizeBytes}\t{attachment.FileName}");
            }
        }

        return builder.ToString().TrimEnd();
    }

    private string BuildUiTestStateDump()
    {
        string? dialogKey = GetResolvedCurrentDialogKey();
        CallSummary? call = currentCallInboxSnapshot?.CurrentCall;
        StringBuilder builder = new("ok");
        builder.AppendLine();
        builder.AppendLine($"shell_mode={windowState.Onboarding.ShellMode}");
        // Почему ядро не поднялось. Без этой строки «shell_mode=Onboarding» неотличимо от «профиль
        // просто не создан», и живой разбор 2026-08-14 упёрся ровно в это.
        builder.AppendLine($"core_init_error={NativeMethods.omni_app_last_init_error().Replace('\r', ' ').Replace('\n', ' ')}");
        builder.AppendLine($"current_dialog={dialogKey ?? "none"}");
        builder.AppendLine($"call_id={call?.CallId ?? "none"}");
        builder.AppendLine($"call_state={call?.State ?? "none"}");
        builder.AppendLine($"call_dialog={call?.DialogKey ?? "none"}");
        builder.AppendLine($"call_direction={call?.Direction ?? "none"}");
        builder.AppendLine($"call_mic_muted={call?.LocalMicMuted.ToString().ToLowerInvariant() ?? "none"}");
        builder.AppendLine($"media_active={callVideoService.IsActiveForDialog(dialogKey).ToString().ToLowerInvariant()}");
        builder.AppendLine($"media_dialog={callVideoService.ActiveDialogKey ?? "none"}");
        builder.AppendLine($"camera_on={callVideoService.IsLocalCameraActive.ToString().ToLowerInvariant()}");
        builder.AppendLine($"camera_usable={callVideoService.LocalCameraUnavailableReason is null}".ToLowerInvariant());
        builder.AppendLine($"screen_sharing={callVideoService.IsScreenSharing.ToString().ToLowerInvariant()}");
        builder.AppendLine($"remote_video={callVideoService.HasRemoteVideo.ToString().ToLowerInvariant()}");
        builder.AppendLine($"audio_muted={callVideoService.IsLocalAudioMuted.ToString().ToLowerInvariant()}");
        builder.AppendLine($"silent_mic={callVideoService.ReadSilentMicrophone() ?? "none"}");
        // СОСТОЯНИЕ СОГЛАСОВАНИЯ, А НЕ ТОЛЬКО «ЗВОНОК ИДЁТ». Без этой строки закрытые ворота
        // входящей медиа (нет выбора -> каждая датаграмма пира выбрасывается) выглядят снаружи
        // ровно как здоровый звонок: оверлей на месте, камера горит, звука нет.
        // Живой разбор 2026-08-30 — docs/bugs/one-button-press-killed-the-call.md.
        builder.AppendLine($"media_negotiation={ReadMediaNegotiationStatusForTests(dialogKey)}");
        builder.AppendLine($"camera_button_enabled={ToggleCurrentCallCameraButton.IsEnabled.ToString().ToLowerInvariant()}");
        builder.AppendLine($"hangup_button_enabled={HangUpCurrentCallButton.IsEnabled.ToString().ToLowerInvariant()}");
        builder.AppendLine($"call_overlay_visible={(CallOverlayRoot.Visibility == Visibility.Visible).ToString().ToLowerInvariant()}");
        // The APPLIED control state, not the computed one: "the service knows video stopped"
        // and "the frozen picture is off the screen" are different claims, and only these
        // three separate them.
        builder.AppendLine($"remote_placeholder_visible={(RemoteCallVideoPlaceholderTextBlock.Visibility == Visibility.Visible).ToString().ToLowerInvariant()}");
        builder.AppendLine($"remote_image_visible={(RemoteCallVideoImage.Visibility == Visibility.Visible).ToString().ToLowerInvariant()}");
        builder.AppendLine($"remote_swapchain_visible={(RemoteCallVideoSwapChainPanel.Visibility == Visibility.Visible).ToString().ToLowerInvariant()}");
        builder.AppendLine($"video_grid_visible={(ChatCallVideoGrid.Visibility == Visibility.Visible).ToString().ToLowerInvariant()}");
        builder.AppendLine($"diagnostics={(CallVideoDiagnosticsTextBlock.Text ?? string.Empty).Replace('\r', ' ').Replace('\n', ' ')}");
        return builder.ToString().TrimEnd();
    }

    private static bool TryParseUiTestResolution(string text, out CallVideoResolutionOption resolution)
    {
        resolution = default;
        string[] parts = text.Split('x', StringSplitOptions.TrimEntries);
        if (parts.Length != 2 ||
            !int.TryParse(parts[0], out int width) ||
            !int.TryParse(parts[1], out int height) ||
            width <= 0 ||
            height <= 0)
        {
            return false;
        }

        resolution = CallVideoResolutionOption.Even(width, height);
        return true;
    }

    /// Статус согласования медиа + есть ли выбор. Выбор — это пропуск для входящих датаграмм
    /// (`dialog_has_active_media_negotiation` в ядре смотрит ровно на него), поэтому обе половины
    /// нужны отдельно: `negotiated` без выбора не бывает, а вот `waiting_for_remote_capabilities`
    /// со свежим локальным объявлением выглядит почти живым и при этом глухой.
    private string ReadMediaNegotiationStatusForTests(string? dialogKey)
    {
        if (string.IsNullOrWhiteSpace(dialogKey))
        {
            return "none";
        }

        try
        {
            RealtimeMediaNegotiationSnapshot snapshot = rustBridge.GetRealtimeMediaNegotiation(dialogKey);
            string selected = snapshot.Selected is null ? "no" : "yes";
            string remote = snapshot.RemoteCapabilities is null ? "no" : "yes";
            return $"{snapshot.Status}\tselected={selected}\tremote_caps={remote}";
        }
        catch (Exception ex)
        {
            return $"error: {ex.GetType().Name}";
        }
    }

    /// <summary>
    /// Разворачивает DTO ядра (`omni_mesh_connection_state`) в строки, которые разбирает скрипт.
    /// Ничего не выводит сам: `link`, `carrier`, `relay_hops` и трасса приходят из ядра как есть —
    /// иначе оболочка начнёт утверждать про связь то, чего ядро не говорило.
    /// </summary>
    private string BuildUiTestMeshStateDump()
    {
        MeshConnectionStateDto mesh;
        try
        {
            mesh = rustBridge.MeshConnectionState();
        }
        catch (Exception error)
        {
            return "error: mesh_connection_state failed: "
                + error.Message.Replace('\r', ' ').Replace('\n', ' ');
        }

        StringBuilder builder = new("ok");
        builder.AppendLine();
        builder.AppendLine($"node_kind={mesh.Node.Kind}");
        builder.AppendLine($"node_endpoints={(mesh.Node.Endpoints is null || mesh.Node.Endpoints.Count == 0
            ? "none"
            : string.Join(",", mesh.Node.Endpoints))}");
        foreach (ContactConnectionDto contact in mesh.Contacts)
        {
            builder.AppendLine(
                $"contact\t{contact.AccountId}\tlink={contact.Link}"
                + $"\tcarrier={contact.Carrier ?? "none"}"
                + $"\trelay_hops={contact.RelayHops?.ToString() ?? "none"}"
                + $"\tverified={contact.Verified.ToString().ToLowerInvariant()}"
                + $"\treason={contact.CannotReachReason ?? "none"}");
            if (contact.Trace is null)
            {
                continue;
            }

            foreach (ConnectionStepDto step in contact.Trace)
            {
                string detail = (step.Detail ?? string.Empty)
                    .Replace('\t', ' ').Replace('\r', ' ').Replace('\n', ' ');
                builder.AppendLine(
                    $"trace\t{contact.AccountId}\t{step.Stage}\t{step.Status}"
                    + $"\t{step.AtMs}\t{detail}");
            }
        }

        return builder.ToString().TrimEnd();
    }
}
