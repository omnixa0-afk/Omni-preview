using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Features.Shared;

namespace Omni.Windows.App;

// The contact's page beside a 1:1 chat (a tap on the header, or «⋯» → «Профиль»), as a group's
// page beside the group (owner, 2026-10-11: «перебрасывает в контакты, давай как в ТГ или как в
// группах»). It is THE page of «Контакты» — moved into the chat while open and back when it
// closes — so there is one card, one set of handlers, not a second copy that drifts.
public sealed partial class MainWindow
{
    private bool contactPageInChat;

    // Held once found: FindName from the Contacts host does not reach it while it is in the chat.
    private StackPanel? movedContactPage;

    private bool contactPanelFollowsWidth;

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
        LayoutChatContactInfo();
        if (!contactPanelFollowsWidth && ChatContactInfoPanel.Parent is FrameworkElement host)
        {
            contactPanelFollowsWidth = true;
            host.SizeChanged += (_, _) => LayoutChatContactInfo();
            ColumnWidth.Details.Changed += LayoutChatContactInfo;
        }
    }

    // The same rule as a group's page: beside the conversation, or over it when there is no room.
    private RailSplitter? chatContactInfoEdge;

    private void LayoutChatContactInfo()
    {
        double width = (ChatContactInfoPanel.Parent as FrameworkElement)?.ActualWidth ?? 0;
        bool sideBySide = width <= 0 || width >= DetailsPanel.SideBySideFrom;
        ChatContactInfoColumn.Width = new GridLength(contactPageInChat && sideBySide ? DetailsPanel.Width : 0);
        Grid.SetColumn(ChatContactInfoPanel, sideBySide ? 1 : 0);
        Grid.SetColumnSpan(ChatContactInfoPanel, sideBySide ? 1 : 2);
        ChatContactInfoPanel.BorderThickness = sideBySide ? new Thickness(1, 0, 0, 0) : new Thickness(0);
        chatContactInfoEdge ??= DetailsPanel.AddEdge(ChatContactInfoPanel);
        chatContactInfoEdge.Visibility = sideBySide ? Visibility.Visible : Visibility.Collapsed;
    }
}
