using System;
using System.Linq;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Omni.Windows.App.Presentation;

namespace Omni.Windows.App.Features.Groups;

/// The invitation card (see the XAML). Answers go straight to the core; a light poll follows the
/// invitation while it is on screen — "joining" until the Welcome lands, then <see cref="Joined"/>;
/// gone without a group (the inviter withdrew it) — says so instead of offering dead buttons.
public sealed partial class GroupInviteCard : UserControl
{
    private RustBridge? bridge;
    private string? groupId;

    private readonly DispatcherTimer refreshTimer = new() { Interval = TimeSpan.FromSeconds(2) };

    public GroupInviteCard()
    {
        InitializeComponent();
        refreshTimer.Tick += (_, _) => Refresh();
        Unloaded += (_, _) => refreshTimer.Stop();
    }

    /// We are in the group now — the shell opens its conversation.
    public event Action<string>? Joined;
    /// Declined — the shell drops the row and closes the card.
    public event Action<string>? Declined;
    public event Action? BackRequested;

    public bool ShowBackButton
    {
        get => BackButton.Visibility == Visibility.Visible;
        set => BackButton.Visibility = value ? Visibility.Visible : Visibility.Collapsed;
    }

    internal void Attach(RustBridge rustBridge) => bridge = rustBridge;

    internal void Show(string id)
    {
        groupId = id;
        ErrorText.Visibility = Visibility.Collapsed;
        Refresh();
        refreshTimer.Start();
    }

    internal void Hide()
    {
        refreshTimer.Stop();
        groupId = null;
    }

    private void Refresh()
    {
        if (bridge is null || groupId is null)
        {
            return;
        }

        try
        {
            if (bridge.ListGroups().Any(g => g.GroupId == groupId && g.MyStatus == "member"))
            {
                string joinedId = groupId;
                Hide();
                Joined?.Invoke(joinedId);
                return;
            }

            GroupInviteView? invite = bridge.ListGroupInvites().FirstOrDefault(i => i.GroupId == groupId);
            if (invite is null)
            {
                Render(name: NameText.Text, inviter: null, status: "gone");
                return;
            }
            Render(invite.Name, invite.InviterName, invite.Status);
        }
        catch (Exception ex)
        {
            ShowError(ex);
        }
    }

    private void Render(string name, string? inviter, string status)
    {
        InitialsText.Text = MainWindowPresentation.BuildAvatarText(name);
        NameText.Text = name;
        bool gone = status == "gone";
        bool joining = status == "accepted";
        InviterText.Text = gone ? "Приглашение больше не действует." : $"Приглашение от {inviter}";
        NoteText.Text = gone
            ? "Его отменили или группа больше недоступна."
            : "Вступив, вы увидите сообщения с момента входа. Участники группы увидят ваше имя.";
        AnswerButtons.Visibility = gone || joining ? Visibility.Collapsed : Visibility.Visible;
        JoiningPanel.Visibility = joining ? Visibility.Visible : Visibility.Collapsed;
        JoiningText.Text = $"Вступаем — ждём подтверждения от {inviter}…";
        // Gone keeps polling: the same group can be offered again, and then the answers come back.
    }

    private void OnJoinClicked(object sender, RoutedEventArgs e)
    {
        if (bridge is null || groupId is null)
        {
            return;
        }

        try
        {
            bridge.AcceptGroupInvite(groupId);
            ErrorText.Visibility = Visibility.Collapsed;
            Refresh();
        }
        catch (Exception ex)
        {
            ShowError(ex);
        }
    }

    private void OnDeclineClicked(object sender, RoutedEventArgs e)
    {
        if (bridge is null || groupId is null)
        {
            return;
        }

        try
        {
            string declinedId = groupId;
            bridge.DeclineGroupInvite(declinedId);
            Hide();
            Declined?.Invoke(declinedId);
        }
        catch (Exception ex)
        {
            ShowError(ex);
        }
    }

    private void OnBackClicked(object sender, RoutedEventArgs e) => BackRequested?.Invoke();

    private void ShowError(Exception ex)
    {
        ErrorText.Text = ex.Message;
        ErrorText.Visibility = Visibility.Visible;
    }
}
