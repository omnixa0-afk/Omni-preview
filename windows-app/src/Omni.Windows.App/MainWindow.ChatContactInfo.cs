using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Omni.Windows.App;

// The contact's page beside a 1:1 chat (a tap on the header, or «⋯» → «Профиль»), as a group's
// page beside the group (owner, 2026-10-11: «перебрасывает в контакты, давай как в ТГ или как в
// группах»). It is THE page of «Контакты» — moved into the chat while open and back when it
// closes — so there is one card, one set of handlers, not a second copy that drifts.
public sealed partial class MainWindow
{
    private const double ChatContactInfoWidth = 380;

    private bool contactPageInChat;

    // Held once found: FindName from the Contacts host does not reach it while it is in the chat.
    private StackPanel? movedContactPage;

    private void OpenCurrentChatContactProfile()
    {
        string? contactId = currentChatHeaderSummary?.ContactCard?.Contact.ContactId;
        if (string.IsNullOrWhiteSpace(contactId))
        {
            return;
        }

        // The header toggles the page, as a group's header does.
        if (contactPageInChat && string.Equals(windowState.SelectedContactId, contactId, StringComparison.Ordinal))
        {
            CloseChatContactInfo();
            return;
        }

        windowState.SelectedContactId = contactId;
        windowState.SelectedContactRevision = null;
        RefreshSelectedContactCard();
        WakeSelectedContactRefresh();
        MoveContactPage(toChat: true);
        UpdateControlStates();
        SaveUiState();
    }

    private void CloseChatContactInfo()
    {
        if (contactPageInChat)
        {
            MoveContactPage(toChat: false);
            UpdateControlStates();
        }
    }

    private void MoveContactPage(bool toChat)
    {
        contactPageInChat = toChat;
        StackPanel page = SelectedContactPage;
        Grid contactsHost = ContactsDetailPageHost;
        ScrollViewer chatScroll = ChatContactInfoScroll;
        contactsHost.Children.Clear();
        chatScroll.Content = null;
        if (toChat)
        {
            chatScroll.Content = page;
        }
        else
        {
            contactsHost.Children.Add(page);
        }
        ChatContactInfoPanel.Visibility = toChat ? Visibility.Visible : Visibility.Collapsed;
        ChatContactInfoColumn.Width = new GridLength(toChat ? ChatContactInfoWidth : 0);
    }
}
