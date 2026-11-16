using Microsoft.UI.Xaml;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Chats;
using Omni.Windows.App.Features.Contacts;

namespace Omni.Windows.App;

// «Помощь» (docs/decisions/omni-app-ux.md, R3): a person in trouble first gets the answer in words and
// the way to what helps; the technical pages sit below it.
public sealed partial class MainWindow
{
    private void WireHelpPage() => DiagnosticsSectionHost.HelpActionRequested += OnHelpAction;

    private void RenderHelpStatus(MeshConnectionStateDto? mesh)
    {
        (string title, string detail, bool problem) = mesh?.Node.Kind switch
        {
            "online_reachable" => ("Связь есть", "Контакты могут подключиться к вам напрямую.", false),
            "online_behind_nat" => ("Связь есть", "Напрямую к вам не попасть, контакты дойдут через других.", false),
            "connecting" => ("Подключаюсь к сети…", "Если это затянулось, проверьте интернет.", false),
            "isolated" => ("Нет связи", "Нет ни локальной сети, ни внешнего адреса. Проверьте интернет. Сообщения уйдут, когда связь появится.", true),
            _ => ("Сеть не запущена", "Если это не проходит, перезапустите Omni.", true),
        };
        DiagnosticsSectionHost.ShowHelpStatus(title, detail, DescribeHelpPeople(mesh), problem);
        DiagnosticsSectionHost.ShowHelpWaiting(HelpWaitingConversations());
    }

    // Who the numbers are about. Why a message waits is said in that chat's header, where the person
    // lands; the list stays short.
    private List<(string DialogKey, string Title, string Text)> HelpWaitingConversations()
    {
        const int Shown = 5;
        List<(string, string, string)> rows = [];
        foreach (DialogQueueDiagnostics dialog in lastQueueDiagnosticsSnapshot?.PendingByDialog ?? [])
        {
            int waiting = dialog.PendingCount; // retries are among them
            if (waiting + dialog.FailedCount == 0
                || chatThreadItems.FirstOrDefault(item => item.DialogKey == dialog.DialogKey) is not { } chat)
            {
                continue;
            }

            string text = dialog.FailedCount > 0 ? $"Ждут отправки: {waiting}. Не отправилось: {dialog.FailedCount}." : $"Ждут отправки: {waiting}.";
            rows.Add((dialog.DialogKey, chat.Title, text));
        }

        return rows.Take(Shown).ToList();
    }

    private string DescribeHelpPeople(MeshConnectionStateDto? mesh)
    {
        List<string> lines = [];
        if (mesh is not null)
        {
            int total = mesh.Contacts.Count;
            int linked = mesh.Contacts.Count(contact => contact.Link is "online_direct" or "online_via_relay");
            bool one = total % 10 == 1 && total % 100 != 11;
            lines.Add(total == 0 ? "Контактов пока нет." : $"На связи {linked} из {total} {(one ? "контакта" : "контактов")}.");
        }

        if (lastQueueDiagnosticsSnapshot is { } queue)
        {
            int waiting = queue.QueuedCount + queue.RetryingCount;
            if (waiting > 0)
            {
                lines.Add($"Ждут отправки: {waiting}.");
            }

            if (queue.FailedCount > 0)
            {
                lines.Add($"Не отправилось: {queue.FailedCount}.");
            }
        }

        return string.Join(' ', lines);
    }

    private void OnHelpAction(string action)
    {
        if (action.StartsWith("chat:", StringComparison.Ordinal))
        {
            string dialogKey = action["chat:".Length..];
            ChatThreadListItem? chat = chatThreadItems.FirstOrDefault(item => item.DialogKey == dialogKey);
            AuthenticatedNavigationView.SelectedItem = ChatsNavigationItem;
            if (chat is { IsGroup: true })
            {
                OpenGroupRow(chat);
            }
            else
            {
                ShowChatsSectionForDialog(dialogKey, chat?.ContactId);
            }

            return;
        }

        switch (action)
        {
            case "codes":
                AuthenticatedNavigationView.SelectedItem = ContactsNavigationItem;
                ApplyContactsWorkspaceMode(ContactsWorkspaceMode.Add);
                ContactsReachCodesExpander.IsExpanded = true;
                ContactsReachCodesExpander.StartBringIntoView();
                break;
            case "chats":
                AuthenticatedNavigationView.SelectedItem = ChatsNavigationItem;
                break;
            default:
                AuthenticatedNavigationView.SelectedItem = SettingsNavigationItem;
                SettingsSectionHost.ShowSection(action);
                break;
        }
    }
}
