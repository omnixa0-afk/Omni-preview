using System;
using System.Threading.Tasks;
using Microsoft.UI.Xaml.Controls;
using Windows.ApplicationModel.DataTransfer;

namespace Omni.Windows.App;

// Model A discoverability (global identity discovery) UI. Settings → "Сеть и обнаружение" shows
// my Omni-ID (a shareable public address that commits to my identity keys) and a Public toggle
// (findable-by-ID opt-in; default off = Model B invite-only). The "add by Omni-ID" field lives in
// Contacts → "Добавить" (moved 2026-07-06, see docs/design/connectivity-ux.md §2.D) alongside the
// other ways to reach a specific person. All request→accept: findable ≠ auto-admitted.
// Self-contained partial that drives the RustBridge discoverability FFI.
// See docs/decisions/global-identity-discovery.md.
public sealed partial class MainWindow
{
    private bool suppressDiscoverabilityToggle;
    private bool discoverabilityStatusIsOmniIdError;

    private void RefreshDiscoverabilitySettings()
    {
        try
        {
            SettingsMyOmniIdTextBox.Text = rustBridge.GetMyOmniId();
            // A refresh before the core was up left "could not get the Omni-ID" under an ID that
            // was by then shown right above it (seen live 2026-10-10).
            if (discoverabilityStatusIsOmniIdError)
            {
                SetDiscoverabilityStatus(string.Empty);
            }
        }
        catch (Exception ex)
        {
            SettingsMyOmniIdTextBox.Text = string.Empty;
            SetDiscoverabilityStatus($"Не удалось получить Omni-ID: {NormalizeUserError(ex)}");
            discoverabilityStatusIsOmniIdError = true;
        }

        try
        {
            bool isPublic = string.Equals(
                rustBridge.GetDiscoverabilityMode(),
                "public",
                StringComparison.OrdinalIgnoreCase);
            suppressDiscoverabilityToggle = true;
            SettingsDiscoverabilityToggle.IsOn = isPublic;
            suppressDiscoverabilityToggle = false;
            RefreshAddByOmniIdSelfHint(isPublic);
            RefreshNameFindableSetting(isPublic);
        }
        catch
        {
            // Leave the toggle as-is if the mode can't be read; the next toggle will set it.
        }
    }

    /// Finding by Omni-ID needs BOTH sides findable. The Contacts pane already says the peer
    /// must have it on; this says whether YOU do, because handing out your ID while it is off
    /// gives someone an address that cannot resolve and nothing used to mention it.
    private void RefreshAddByOmniIdSelfHint(bool isPublic)
    {
        try
        {
            AddByOmniIdSelfHintTextBlock.Text = isPublic
                ? string.Empty
                : "Вас самих по ID сейчас не найти: «Находить по Omni-ID» в настройках выключено. На поиск чужого ID это не влияет.";
            AddByOmniIdSelfHintTextBlock.Visibility = isPublic
                ? Microsoft.UI.Xaml.Visibility.Collapsed
                : Microsoft.UI.Xaml.Visibility.Visible;
        }
        catch
        {
            // The contacts section may not be realised yet; the next refresh picks it up.
        }
    }

    private void OnCopyOmniIdClicked(object sender, Microsoft.UI.Xaml.RoutedEventArgs e)
    {
        string id = SettingsMyOmniIdTextBox.Text ?? string.Empty;
        if (string.IsNullOrWhiteSpace(id))
        {
            return;
        }

        DataPackage package = new();
        package.SetText(id);
        Clipboard.SetContent(package);
        SetDiscoverabilityStatus("Omni-ID скопирован. Дайте его собеседнику.");
    }

    private void OnDiscoverabilityToggled(object sender, Microsoft.UI.Xaml.RoutedEventArgs e)
    {
        if (suppressDiscoverabilityToggle)
        {
            return;
        }

        bool isPublic = SettingsDiscoverabilityToggle.IsOn;
        try
        {
            rustBridge.SetDiscoverabilityMode(isPublic ? "public" : "off");
            RefreshAddByOmniIdSelfHint(isPublic);
            RefreshNameFindableSetting(isPublic);
            SetDiscoverabilityStatus(isPublic
                ? "Включено. Через пару минут вас можно будет найти; заявки от незнакомцев ждут вашего «да»."
                : "Выключено. Через пару минут по ID вас будет не найти — только по приглашению.");
        }
        catch (Exception ex)
        {
            // Revert the visual toggle to the persisted state on failure.
            suppressDiscoverabilityToggle = true;
            SettingsDiscoverabilityToggle.IsOn = !isPublic;
            suppressDiscoverabilityToggle = false;
            SetDiscoverabilityStatus($"Не удалось изменить: {NormalizeUserError(ex)}");
        }
    }

    private async void OnAddByOmniIdClicked(object sender, Microsoft.UI.Xaml.RoutedEventArgs e)
    {
        string id = AddByOmniIdTextBox.Text?.Trim() ?? string.Empty;
        if (id.Length == 0)
        {
            SetAddByOmniIdStatus("Введите Omni-ID (omni:…) или имя человека.");
            return;
        }

        if (!LooksLikeOmniId(id))
        {
            AddByOmniIdButton.IsEnabled = false;
            try
            {
                await SearchPeopleByNameAsync(id);
            }
            finally
            {
                AddByOmniIdButton.IsEnabled = true;
            }

            return;
        }

        ClearFoundPeople();
        AddByOmniIdButton.IsEnabled = false;
        SetAddByOmniIdStatus("Ищу в сети по Omni-ID…");
        try
        {
            await Task.Run(() => rustBridge.FindContactByOmniId(id, string.Empty));
            AddByOmniIdTextBox.Text = string.Empty;
            // Honest wording: the core found + verified the beacon and started dialing the peer's
            // advertised endpoints, but delivery is asynchronous (link-up → hello → bootstrap that
            // carries the request). We cannot claim "отправлена" synchronously — say we're
            // establishing the connection, which is what actually happens now.
            SetAddByOmniIdStatus("Собеседник найден. Устанавливаем связь и отправляем заявку — он получит её, как только соединение поднимется.");
            RefreshContactsList(force: true, selectContactId: null);
        }
        catch (Exception ex)
        {
            SetAddByOmniIdStatus($"Не удалось найти по ID: {NormalizeUserError(ex)}");
        }
        finally
        {
            AddByOmniIdButton.IsEnabled = true;
        }
    }

    private void SetDiscoverabilityStatus(string text)
    {
        SettingsDiscoverabilityStatusTextBlock.Text = text;
        discoverabilityStatusIsOmniIdError = false;
    }

    private void SetAddByOmniIdStatus(string text)
    {
        AddByOmniIdStatusTextBlock.Text = text;
    }
}
