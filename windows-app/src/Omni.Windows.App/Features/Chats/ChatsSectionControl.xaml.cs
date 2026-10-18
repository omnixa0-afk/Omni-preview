using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Omni.Windows.App.Features.Chats;

public sealed partial class ChatsSectionControl : UserControl
{
    private string currentLayoutState = string.Empty;
    private double lastLayoutWidth;

    public ChatsSectionControl()
    {
        InitializeComponent();
        Loaded += OnChatsSectionLoaded;
        // One left-column width for every section (Shared.SectionRail).
        Loaded += (_, _) =>
        {
            Shared.SectionRail.Changed -= ApplyRailWidth;
            Shared.SectionRail.Changed += ApplyRailWidth;
            ApplyRailWidth();
        };
        Unloaded += (_, _) => Shared.SectionRail.Changed -= ApplyRailWidth;
        SizeChanged += OnChatsSectionSizeChanged;
        ChatsListView.SelectionChanged += OnChatsListSelectionChangedForLayout;
        ChatHeaderGrid.BackRequested += (_, _) => BackToListRequested?.Invoke(this, EventArgs.Empty);
        // The ids UI automation knows the 1:1 parts by.
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetAutomationId(ChatHeaderGrid.BackButton, "ChatBackToListButton");
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetAutomationId(ChatComposerBorder.Input, "MessageInputTextBox");
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetAutomationId(ChatComposerBorder.SendButton, "SendButton");
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetAutomationId(ChatComposerBorder.AttachButton, "AttachmentMenuButton");
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(ChatHeaderGrid.IdentityButton, "Профиль контакта");
        GroupConversationHost.BackRequested += () => BackToListRequested?.Invoke(this, EventArgs.Empty);
        GroupInviteCardHost.BackRequested += () => BackToListRequested?.Invoke(this, EventArgs.Empty);
    }

    internal Groups.GroupConversationControl GroupConversation => GroupConversationHost;
    internal Groups.GroupInviteCard GroupInviteCard => GroupInviteCardHost;

    internal enum GroupPane
    {
        None,
        Conversation,
        Invite,
    }

    /// What fills the conversation pane: the 1:1 dialog (None), a group conversation, or an
    /// invitation card. The hidden group views stop their polling.
    internal void ShowGroupPane(GroupPane pane)
    {
        DirectChatPanel.Visibility = pane == GroupPane.None ? Visibility.Visible : Visibility.Collapsed;
        GroupConversationHost.Visibility = pane == GroupPane.Conversation ? Visibility.Visible : Visibility.Collapsed;
        GroupInviteCardHost.Visibility = pane == GroupPane.Invite ? Visibility.Visible : Visibility.Collapsed;
        if (pane != GroupPane.Conversation)
        {
            GroupConversationHost.Hide();
        }
        if (pane != GroupPane.Invite)
        {
            GroupInviteCardHost.Hide();
        }
    }

    public event RoutedEventHandler? AttachmentCardClicked;

    /// A tap on a photo in a bubble (Tag = the message id): the host opens it on the whole window.
    public event RoutedEventHandler? AttachmentPhotoClicked;

    private void OnAttachmentPhotoClicked(object sender, RoutedEventArgs e)
        => AttachmentPhotoClicked?.Invoke(sender, e);

    // A flyout is outside the bubble's tree: its items get the message id from the photo they opened on.
    private void OnPhotoMenuOpening(object? sender, object e)
    {
        if (sender is MenuFlyout { Target: FrameworkElement { Tag: string messageId } } menu)
        {
            foreach (MenuFlyoutItemBase item in menu.Items)
            {
                item.Tag = messageId;
            }
        }
    }

    /// «Сохранить как…» on a photo's menu (Tag = the message id).
    public event RoutedEventHandler? AttachmentPhotoSaveClicked;

    private void OnAttachmentPhotoSaveClicked(object sender, RoutedEventArgs e)
        => AttachmentPhotoSaveClicked?.Invoke(sender, e);

    // Raised when the user taps "back" from an open conversation in the narrow
    // master-detail layout. The host (MainWindow) closes the current chat, which
    // clears the selection and returns this section to the list pane.
    public event EventHandler? BackToListRequested;

    private void OnAttachmentCardClicked(object sender, RoutedEventArgs e)
    {
        AttachmentCardClicked?.Invoke(sender, e);
    }

    private void OnChatsSectionLoaded(object sender, RoutedEventArgs e)
        => ApplyResponsiveLayout(ActualWidth);

    private void OnChatsSectionSizeChanged(object sender, SizeChangedEventArgs e)
        => ApplyResponsiveLayout(e.NewSize.Width);

    // At narrow widths the section is a phone-style master-detail: opening or
    // closing a conversation flips which pane fills the surface, so re-run layout
    // whenever the selected chat changes.
    private void OnChatsListSelectionChangedForLayout(object sender, SelectionChangedEventArgs e)
        => ApplyResponsiveLayout(lastLayoutWidth > 0 ? lastLayoutWidth : ActualWidth);

    // Drive the rail/conversation layout off THIS control's width, not the window
    // width. The section is hosted inside NavigationView.Content, whose pane eats
    // up to 228px; an AdaptiveTrigger on MinWindowWidth over-reports the space the
    // section actually has and keeps the rail column too wide at half-screen widths
    // (см. docs/architecture/ui-audit.md §D).
    //
    // Below the narrow threshold the section becomes a master-detail: the chat list
    // and the open conversation each take the whole surface (never stacked). Stacking
    // them starved the message list of height — the conversation card's fixed rows
    // (header + session + composer) consumed nearly all of its share, collapsing the
    // thread to ~0px (см. docs/bugs/chats-history-collapse-stacked-layout.md).
    // Only the two-column layouts have a rail; the narrow ones are one pane, full width.
    private void ApplyRailWidth()
    {
        if (currentLayoutState is "ChatsWide" or "ChatsMedium")
        {
            ChatsRailColumn.Width = new GridLength(Shared.SectionRail.Width);
        }
    }

    private void ApplyResponsiveLayout(double width)
    {
        if (width > 0)
        {
            lastLayoutWidth = width;
        }

        bool conversationOpen = ChatsListView?.SelectedItem is not null;
        string target = width >= 1040
            ? "ChatsWide"
            : width >= 720
                ? "ChatsMedium"
                : conversationOpen
                    ? "ChatsNarrowConversation"
                    : "ChatsNarrowList";

        if (target == currentLayoutState)
        {
            return;
        }

        currentLayoutState = target;
        VisualStateManager.GoToState(this, target, useTransitions: false);
        ApplyRailWidth();
        ChatHeaderGrid.ShowBack = target == "ChatsNarrowConversation";
        GroupConversationHost.ShowBackButton = target == "ChatsNarrowConversation";
        GroupInviteCardHost.ShowBackButton = target == "ChatsNarrowConversation";
    }
}
