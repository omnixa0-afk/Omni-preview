using System;
using System.Collections.Generic;
using System.Threading.Tasks;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace Omni.Windows.App;

// Remote access, owner side (docs/decisions/remote-access-for-contacts.md): the "Удалённый доступ"
// block in the contact card. It only edits the per-contact grant — who may control this computer and
// in which mode, behind a warning (what the contact gets and does not; an extra acknowledgement for
// "without a question"). Starting a session is a separate affordance; nothing here captures input.
public sealed partial class MainWindow
{
    // Set while we populate the controls from state, so the Toggled/Checked handlers don't fire back
    // into the core on our own writes.
    private bool suppressRemoteAccessEvents;

    private const string RemoteAccessModeAsk = "ask";
    private const string RemoteAccessModeUnattended = "unattended";

    private void RenderSelectedContactRemoteAccess()
    {
        if (selectedContactCard is null)
        {
            return; // the whole detail body is hidden with no selection
        }

        bool verified = string.Equals(selectedContactCard.TrustState, "verified", StringComparison.OrdinalIgnoreCase);
        string contactId = selectedContactCard.Contact.ContactId;
        string name = selectedContactCard.Contact.Petname ?? "контакт";

        Core.ContactRemoteAccessRecord? access = null;
        try
        {
            access = rustBridge.GetContactRemoteAccess(contactId);
        }
        catch (Exception)
        {
            // Reading the setting failed (profile not ready): show it off, let the next render retry.
        }

        string? grantedMode = access?.GrantedMode;
        bool granted = grantedMode is not null;

        suppressRemoteAccessEvents = true;
        try
        {
            // The row names the setting; the switch and the hint under it say its state.
            ContactRemoteAccessTitleTextBlock.Text = "Удалённый доступ к этому компьютеру";
            // An unverified contact cannot be granted — an unverified key could be an impostor's. A
            // grant already in place stays visible so it can be turned off.
            ContactRemoteAccessToggle.IsEnabled = verified || granted;
            ContactRemoteAccessToggle.IsOn = granted;
            ContactRemoteAccessModePanel.Visibility = granted ? Visibility.Visible : Visibility.Collapsed;
            ContactRemoteAccessAskRadio.IsChecked = grantedMode != RemoteAccessModeUnattended;
            ContactRemoteAccessUnattendedRadio.IsChecked = grantedMode == RemoteAccessModeUnattended;
            ContactRemoteAccessHintTextBlock.Text = BuildRemoteAccessHint(verified, access, name);
            bool online = selectedContactCard.Contact.RemoteAccountId is { } account && CurrentlyLinkedAccountIds().Contains(account);
            string? received = BuildRemoteAccessReceivedText(name, access?.ReceivedMode, online);
            ContactRemoteAccessReceivedTextBlock.Text = received ?? string.Empty;
            ContactRemoteAccessReceivedTextBlock.Visibility = received is null ? Visibility.Collapsed : Visibility.Visible;
            string? lastSession = BuildLastSessionText(access?.LastSessionAtUnixMs, DateTimeOffset.Now);
            ContactRemoteAccessLastSessionTextBlock.Text = lastSession ?? string.Empty;
            ContactRemoteAccessLastSessionTextBlock.Visibility = lastSession is null ? Visibility.Collapsed : Visibility.Visible;
        }
        finally
        {
            suppressRemoteAccessEvents = false;
        }

        RenderRemoteControlAffordance(StartSelectedContactRemoteControlButton, selectedContactCard.Contact);
    }

    internal static string BuildRemoteAccessHint(bool verified, Core.ContactRemoteAccessRecord? access, string name = "контакт")
    {
        if (!verified && (access?.GrantedMode is null))
        {
            return "Чтобы разрешить управление, сначала проверьте контакт в разделе «Безопасность».";
        }

        if (access?.GrantedMode == RemoteAccessModeUnattended)
        {
            return "Контакт может подключиться без вопроса, в том числе когда вас нет рядом.";
        }

        if (access?.GrantedMode == RemoteAccessModeAsk)
        {
            return "Каждое подключение нужно будет подтвердить на этом компьютере.";
        }

        return $"Выключено: «{name}» не может управлять этим компьютером.";
    }

    /// The other direction: this contact lets us control ITS computer. Null when it does not.
    internal static string? BuildRemoteAccessReceivedText(string name, string? receivedMode, bool online)
    {
        if (receivedMode is null)
        {
            return null;
        }

        string how = receivedMode == RemoteAccessModeUnattended ? "без вопроса" : "с подтверждением каждый раз";
        string now = online
            ? "Нажмите «Управлять компьютером», чтобы подключиться."
            : "Сейчас он не в сети — подключиться можно, когда появится.";
        return $"«{name}» разрешил вам управлять своим компьютером ({how}). {now}";
    }

    /// «Последний сеанс: сегодня в 14:02» — either direction. Null when there was none.
    internal static string? BuildLastSessionText(long? lastSessionUnixMs, DateTimeOffset now)
    {
        if (lastSessionUnixMs is not long ms)
        {
            return null;
        }

        DateTimeOffset at = DateTimeOffset.FromUnixTimeMilliseconds(ms).ToOffset(now.Offset);
        string when = at.Date == now.Date
            ? $"сегодня в {at:HH:mm}"
            : at.Date == now.Date.AddDays(-1) ? $"вчера в {at:HH:mm}" : $"{at:dd.MM.yyyy} в {at:HH:mm}";
        return $"Последний сеанс: {when}";
    }

    private void OnContactRemoteAccessToggled(object sender, RoutedEventArgs e)
    {
        if (suppressRemoteAccessEvents || selectedContactCard is null)
        {
            return;
        }

        if (!ContactRemoteAccessToggle.IsOn)
        {
            ApplyRemoteAccessGrant(null); // turning off is never asked about
            return;
        }

        // Turning on keeps the current mode choice, defaulting to the safer "ask" — and goes through
        // the warning first.
        string mode = ContactRemoteAccessUnattendedRadio.IsChecked == true ? RemoteAccessModeUnattended : RemoteAccessModeAsk;
        _ = ConfirmThenApplyRemoteAccessGrantAsync(mode);
    }

    private void OnContactRemoteAccessModeChanged(object sender, RoutedEventArgs e)
    {
        if (suppressRemoteAccessEvents || selectedContactCard is null || !ContactRemoteAccessToggle.IsOn)
        {
            return;
        }

        if (ContactRemoteAccessUnattendedRadio.IsChecked == true)
        {
            _ = ConfirmThenApplyRemoteAccessGrantAsync(RemoteAccessModeUnattended); // a step up: warn again
            return;
        }

        ApplyRemoteAccessGrant(RemoteAccessModeAsk);
    }

    // The warning before access is given (or raised to "without a question"). Declined or dismissed,
    // nothing changes and the controls snap back to the stored state.
    private async Task ConfirmThenApplyRemoteAccessGrantAsync(string mode)
    {
        if (selectedContactCard is null)
        {
            return;
        }

        string contactId = selectedContactCard.Contact.ContactId;
        bool unattended = mode == RemoteAccessModeUnattended;
        RemoteAccessWarning warning = BuildRemoteAccessWarning(selectedContactCard.Contact.Petname ?? "контакт", unattended);
        var body = new StackPanel { Spacing = 8 };
        foreach (string paragraph in warning.Paragraphs)
        {
            body.Children.Add(new TextBlock { Text = paragraph, TextWrapping = TextWrapping.WrapWholeWords });
        }

        ContentDialog dialog = new()
        {
            Title = warning.Title,
            Content = body,
            PrimaryButtonText = "Разрешить",
            CloseButtonText = "Отмена",
            DefaultButton = ContentDialogButton.Close,
            XamlRoot = Content.XamlRoot,
        };
        if (warning.Acknowledgement is not null)
        {
            var acknowledge = new CheckBox { Content = warning.Acknowledgement };
            dialog.IsPrimaryButtonEnabled = false;
            acknowledge.Checked += (_, _) => dialog.IsPrimaryButtonEnabled = true;
            acknowledge.Unchecked += (_, _) => dialog.IsPrimaryButtonEnabled = false;
            body.Children.Add(acknowledge);
        }

        bool confirmed;
        try
        {
            confirmed = await dialog.ShowAsync() == ContentDialogResult.Primary;
        }
        catch (Exception)
        {
            confirmed = false; // another dialog was open: treat as not confirmed, nothing granted
        }

        // The selection may have moved while the dialog was up; grant only to the contact it named.
        if (confirmed && selectedContactCard?.Contact.ContactId == contactId)
        {
            ApplyRemoteAccessGrant(mode);
        }
        else
        {
            RenderSelectedContactRemoteAccess();
        }
    }

    internal sealed record RemoteAccessWarning(string Title, IReadOnlyList<string> Paragraphs, string? Acknowledgement);

    internal static RemoteAccessWarning BuildRemoteAccessWarning(string name, bool unattended)
    {
        var paragraphs = new List<string>
        {
            $"«{name}» сможет видеть весь экран этого компьютера и управлять мышью и клавиатурой — всё, что здесь открыто.",
            "Не сможет: экран блокировки и окна, запущенные от администратора, — Windows не отдаёт их чужому вводу.",
            unattended
                ? "Подключение начнётся без вопроса. Пока идёт сеанс, поверх всех окон будет плашка «Остановить»."
                : "Каждое подключение нужно будет подтвердить здесь. Пока идёт сеанс, поверх всех окон будет плашка «Остановить».",
            "Отключить можно в любой момент — здесь же или в Настройках («Отозвать у всех»).",
        };
        return new RemoteAccessWarning(
            unattended ? $"Разрешить «{name}» подключаться без вопроса?" : $"Разрешить «{name}» управлять этим компьютером?",
            paragraphs,
            unattended ? $"Я понимаю, что «{name}» сможет подключиться, когда меня нет рядом" : null);
    }

    private void ApplyRemoteAccessGrant(string? mode)
    {
        if (selectedContactCard is null)
        {
            return;
        }

        try
        {
            rustBridge.SetContactRemoteAccessGranted(selectedContactCard.Contact.ContactId, mode);
            ContactManagementStatusTextBlock.Text = mode is null
                ? "Удалённый доступ отключён."
                : "Удалённый доступ разрешён.";
            if (mode is not null)
            {
                _ = OfferBackgroundModeAfterGrantAsync(selectedContactCard.Contact.Petname ?? "контакт");
            }
        }
        catch (Exception ex)
        {
            ContactManagementStatusTextBlock.Text = $"Не удалось изменить удалённый доступ: {NormalizeUserError(ex)}";
        }

        RenderSelectedContactRemoteAccess();
    }
}
