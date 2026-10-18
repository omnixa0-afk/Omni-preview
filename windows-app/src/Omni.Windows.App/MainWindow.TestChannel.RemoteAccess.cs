using System.Text;
using System.Text.Json;
using Microsoft.UI.Xaml;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Contacts;

namespace Omni.Windows.App;

/// <summary>
/// UI-test-channel commands for standing remote access (docs/decisions/remote-access-for-contacts.md).
/// Same handlers as the buttons. The contact is the one of the open chat (`chat.open` first).
///   verify.mine                 — my verification payload, base64 (hand it to the other machine)
///   verify.confirm &lt;base64&gt;     — confirm the open chat's contact against that payload
///   ra.grant ask|unattended|off — the contact card switch (no warning dialog: scripts cannot click)
///   rc.request                  — "Управлять компьютером"
///   rc.state                    — session role/phase, sharing, control, the access record
/// </summary>
public sealed partial class MainWindow
{
    private string? TryExecuteRemoteAccessUiTestCommand(string command, IReadOnlyList<string> args)
    {
        ContactSnapshot? contact = currentChatHeaderSummary?.ContactCard?.Contact;
        switch (command)
        {
            case "verify.mine":
                return Convert.ToBase64String(Encoding.UTF8.GetBytes(JsonSerializer.Serialize(
                    rustBridge.GetMyVerificationPayload(), OmniJsonSerializerContext.Default.MyVerificationPayloadV1)));

            case "verify.confirm":
            {
                if (contact is null || args.Count == 0)
                {
                    return "error: verify.confirm needs an open chat and a payload";
                }

                ContactVerificationPayloadV1? payload = JsonSerializer.Deserialize(
                    Encoding.UTF8.GetString(Convert.FromBase64String(args[0])),
                    OmniJsonSerializerContext.Default.ContactVerificationPayloadV1);
                ContactCardSnapshot card = rustBridge.ConfirmContactVerification(contact.ContactId, payload!, "manual_compare");
                return $"ok trust={card.TrustState}";
            }

            case "ra.grant":
            {
                if (contact is null || args.Count == 0)
                {
                    return "error: ra.grant needs an open chat and ask|unattended|off";
                }

                string? mode = args[0] == "off" ? null : args[0];
                ContactRemoteAccessRecord record = rustBridge.SetContactRemoteAccessGranted(contact.ContactId, mode);
                return $"ok granted={record.GrantedMode ?? "none"}";
            }

            case "rc.request":
                StartRemoteControlWith(contact);
                RefreshCallInbox();
                return BuildRemoteControlUiTestState();

            // Navigation for screenshots of the real UI (the same selection the list click makes).
            // Contacts → «Добавить»: the add page, as the button opens it.
            case "ui.contacts.add":
                AuthenticatedNavigationView.SelectedItem = ContactsNavigationItem;
                ApplyContactsWorkspaceMode(ContactsWorkspaceMode.Add);
                return "ok";

            case "ui.contacts":
            {
                ApplyContactsWorkspaceMode(ContactsWorkspaceMode.Directory);
                AuthenticatedNavigationView.SelectedItem = ContactsNavigationItem;
                if (args.Count > 0)
                {
                    foreach (object item in FilteredContactsListView.Items)
                    {
                        if (item is ContactListItem listed && listed.Title.StartsWith(string.Join(' ', args), StringComparison.OrdinalIgnoreCase))
                        {
                            FilteredContactsListView.SelectedItem = listed;
                            break;
                        }
                    }
                }

                return "ok";
            }

            case "ui.settings":
                AuthenticatedNavigationView.SelectedItem = SettingsNavigationItem;
                // ui.settings <section> opens it (profile|devices|privacy|network|calls|general|advanced);
                // remote is a group of «Приватность», couriers of «Связь».
                if (args.Count > 0)
                {
                    SettingsSectionHost.ShowSection(args[0] switch
                    {
                        "remote" => "privacy",
                        "couriers" => "network",
                        "relays" => "network",
                        "plans" => "network",
                        "plans-name" => "privacy",
                        string section => section,
                    });
                }

                if (args.Count > 0 && args[0] == "remote")
                {
                    SettingsRemoteAccessCard.StartBringIntoView(new Microsoft.UI.Xaml.BringIntoViewOptions { VerticalAlignmentRatio = 0.1 });
                }

                if (args.Count > 0 && args[0] == "devices")
                {
                    SettingsDevicesCard.StartBringIntoView(new Microsoft.UI.Xaml.BringIntoViewOptions { VerticalAlignmentRatio = 0.1 });
                }

                if (args.Count > 0 && args[0] == "plans-name")
                {
                    Expander namePlan = ResolveSectionElement<Expander>(SettingsSectionHost, "SettingsNamePlanExpander");
                    namePlan.IsExpanded = true;
                    namePlan.StartBringIntoView(new Microsoft.UI.Xaml.BringIntoViewOptions { VerticalAlignmentRatio = 0.05 });
                }

                if (args.Count > 0 && args[0] == "plans")
                {
                    // The contacts' step editor, opened: «Где объявлять» under «Почтовые ящики».
                    Expander plan = ResolveSectionElement<Expander>(SettingsSectionHost, "SettingsContactsPlanExpander");
                    plan.IsExpanded = true;
                    plan.StartBringIntoView(new Microsoft.UI.Xaml.BringIntoViewOptions { VerticalAlignmentRatio = 0.05 });
                }

                if (args.Count > 0 && args[0] == "relays")
                {
                    SettingsFindRelaysButton.StartBringIntoView(new Microsoft.UI.Xaml.BringIntoViewOptions { VerticalAlignmentRatio = 0.1 });
                }

                if (args.Count > 0 && args[0] == "couriers")
                {
                    SettingsRendezvousProvidersCard.StartBringIntoView(new Microsoft.UI.Xaml.BringIntoViewOptions { VerticalAlignmentRatio = 0.05 });
                }

                return "ok";

            // The chat-info panel's session facts, fresh from the core (not the rendered copy).
            case "chat.session":
            {
                string? dialogKey = GetResolvedCurrentDialogKey();
                if (dialogKey is null)
                {
                    return "error: chat.session needs an open chat";
                }

                ChatSessionSummary s = rustBridge.GetChatHeaderScreenSnapshot(dialogKey).SessionSummary;
                long now = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
                return $"ok\nstate={s.SessionStateValue} lease={s.IsOpenLease} paused={s.AutoReconnectPaused}"
                    + $"\nkeepalive_age_ms={(s.LastConfirmedKeepaliveAtUnixMs is long k ? now - k : -1)}"
                    + $" interaction={s.LastInteractionKindValue ?? "none"} rtt={s.LastRoundTripMs?.ToString() ?? "none"}"
                    + $"\npeer={s.PeerEndpoint ?? "none"} local={s.LocalEndpoint ?? "none"}"
                    // What the person sees right now (rendered copies, may lag the core on purpose).
                    + $"\nheader={ChatConnectionStatusTextBlock.Text} | {ChatIdentitySubtitleTextBlock.Text}";
            }

            case "ui.chats":
                AuthenticatedNavigationView.SelectedItem = ChatsNavigationItem;
                return "ok";

            case "rc.state":
                RefreshCallInbox();
                return BuildRemoteControlUiTestState();

            default:
                return null;
        }
    }

    private string BuildRemoteControlUiTestState()
    {
        CallSummary? call = currentCallInboxSnapshot?.CurrentCall;
        StringBuilder builder = new("ok");
        builder.AppendLine();
        builder.AppendLine($"call={call?.CallId ?? "none"} state={call?.State ?? "none"} direction={call?.Direction ?? "none"}");
        builder.AppendLine(call?.RemoteControl is { } session
            ? $"session role={session.Role} phase={session.Phase}"
            : "session=none");
        builder.AppendLine($"sharing={callVideoService.IsScreenSharing.ToString().ToLowerInvariant()} mic_disabled={callVideoService.MicrophoneDisabled.ToString().ToLowerInvariant()}");
        builder.AppendLine($"controlling={(screenControlViewer is not null).ToString().ToLowerInvariant()} view_only={remoteControlViewOnly.ToString().ToLowerInvariant()}");
        builder.AppendLine($"bar_visible={(remoteControlBar?.IsVisible == true).ToString().ToLowerInvariant()} window_visible={AppWindow.IsVisible.ToString().ToLowerInvariant()}");
        builder.AppendLine($"status={ActionStatusTextBlock.Text}");
        builder.AppendLine($"peer_state={callScreenControlState.PeerState ?? "none"} notice={(RemoteControlNoticeBorder.Visibility == Visibility.Visible ? RemoteControlNoticeText.Text : "none")}");
        builder.AppendLine($"overlay_state={ChatCallStateTextBlock.Text}");
        ContactSnapshot? contact = currentChatHeaderSummary?.ContactCard?.Contact;
        if (contact is not null)
        {
            ContactRemoteAccessRecord access = rustBridge.GetContactRemoteAccess(contact.ContactId);
            builder.AppendLine($"access granted={access.GrantedMode ?? "none"} received={access.ReceivedMode ?? "none"} last_session={access.LastSessionAtUnixMs?.ToString() ?? "none"}");
            builder.AppendLine($"header_button={StartCurrentChatRemoteControlButton.Visibility} enabled={StartCurrentChatRemoteControlButton.IsEnabled}");
        }

        return builder.ToString().TrimEnd();
    }
}
