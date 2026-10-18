using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media.Imaging;
using Omni.Windows.App.Core;

namespace Omni.Windows.App;

// Find by name (docs/decisions/find-by-name.md). Settings → Privacy: "Находить меня по имени", off by
// default and usable only while "Находить меня по Omni-ID" is on (a name leads to the Omni-ID). Contacts →
// Add: one field for an Omni-ID or a name; a name shows who published it — photo, name, the start of the
// Omni-ID to tell namesakes apart — and the request goes from the row the user picks, along the
// find-by-Omni-ID path, with the person's name as the contact's name.
public sealed partial class MainWindow
{
    /// The list shows this many people at a time; "Показать ещё" adds as many again.
    private const int FindPeoplePageSize = 20;

    private bool suppressNameFindableToggle;
    private IReadOnlyList<FoundByName> foundPeople = [];
    private int foundPeopleShown;

    private void RefreshNameFindableSetting(bool omniIdPublic)
    {
        try
        {
            bool on = rustBridge.GetNameFindable();
            suppressNameFindableToggle = true;
            SettingsNameFindableToggle.IsOn = on && omniIdPublic;
            suppressNameFindableToggle = false;
            SettingsNameFindableToggle.IsEnabled = omniIdPublic;
            SettingsNameFindableStatusTextBlock.Text = omniIdPublic
                ? string.Empty
                : "Сначала включите «Находить меня по Omni-ID»: имя ведёт к нему.";
        }
        catch
        {
            // The core may not be up yet; the next refresh reads it.
        }
    }

    private void OnNameFindableToggled(object sender, RoutedEventArgs e)
    {
        if (suppressNameFindableToggle)
        {
            return;
        }

        bool on = SettingsNameFindableToggle.IsOn;
        try
        {
            rustBridge.SetNameFindable(on);
            SettingsNameFindableStatusTextBlock.Text = on
                ? "Включено. Через пару минут вас можно будет найти по имени из профиля."
                : "Выключено. Через пару минут по имени вас будет не найти.";
        }
        catch (Exception ex)
        {
            suppressNameFindableToggle = true;
            SettingsNameFindableToggle.IsOn = !on;
            suppressNameFindableToggle = false;
            SettingsNameFindableStatusTextBlock.Text = $"Не удалось изменить: {NormalizeUserError(ex)}";
        }
    }

    /// An Omni-ID as people paste it: `omni:` and 40 hex digits, or the 40 digits alone.
    internal static bool LooksLikeOmniId(string text)
    {
        string hex = text.StartsWith("omni:", StringComparison.OrdinalIgnoreCase) ? text[5..] : text;
        return hex.Length == 40 && hex.All(Uri.IsHexDigit);
    }

    private async Task SearchPeopleByNameAsync(string query)
    {
        ClearFoundPeople();
        SetAddByOmniIdStatus($"Ищу «{query}»…");
        NameSearchResult result;
        try
        {
            result = await Task.Run(() => rustBridge.SearchPeopleByName(query));
        }
        catch (Exception ex)
        {
            SetAddByOmniIdStatus($"Не удалось найти: {NormalizeUserError(ex)}");
            return;
        }

        SetAddByOmniIdStatus(result.Status switch
        {
            "found" => result.People.Count == 1
                ? "Нашёлся один человек. Убедитесь по фото и началу Omni-ID, что это он."
                : $"Нашлось: {result.People.Count}. Узнайте нужного по фото и началу Omni-ID — однофамильцы бывают.",
            "nobody" => "Под этим именем никого нет. Имя ищется точно, как человек записал его у себя (можно только имя или только фамилию), и только у тех, кто включил «Находить меня по имени».",
            "unreachable" => "Не удалось спросить сеть: ни один курьер не ответил. Попробуйте чуть позже.",
            "too_short" => "Введите хотя бы две буквы.",
            _ => string.Empty,
        });
        foundPeople = result.People;
        ShowMoreFoundPeople();
    }

    private void ClearFoundPeople()
    {
        foundPeople = [];
        foundPeopleShown = 0;
        FindPeopleResultsPanel.Children.Clear();
        FindPeopleMoreButton.Visibility = Visibility.Collapsed;
    }

    private void OnFindPeopleMoreClicked(object sender, RoutedEventArgs e) => ShowMoreFoundPeople();

    private void ShowMoreFoundPeople()
    {
        foreach (FoundByName person in foundPeople.Skip(foundPeopleShown).Take(FindPeoplePageSize))
        {
            FindPeopleResultsPanel.Children.Add(BuildFoundPersonRow(person));
        }

        foundPeopleShown = Math.Min(foundPeople.Count, foundPeopleShown + FindPeoplePageSize);
        int left = foundPeople.Count - foundPeopleShown;
        FindPeopleMoreButton.Visibility = left > 0 ? Visibility.Visible : Visibility.Collapsed;
        FindPeopleMoreButton.Content = $"Показать ещё ({left})";
    }

    private FrameworkElement BuildFoundPersonRow(FoundByName person)
    {
        PersonPicture picture = new()
        {
            Width = 40,
            Height = 40,
            DisplayName = person.DisplayName,
            ProfilePicture = BuildPhoto(person.PhotoJpegB64),
        };
        StackPanel text = new() { VerticalAlignment = VerticalAlignment.Center, Spacing = 1 };
        text.Children.Add(new TextBlock
        {
            Text = person.DisplayName,
            FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
            TextTrimming = TextTrimming.CharacterEllipsis,
        });
        // The start and the end of the Omni-ID: enough to tell two namesakes apart, short enough to read.
        string hex = person.OmniId.StartsWith("omni:", StringComparison.Ordinal) ? person.OmniId[5..] : person.OmniId;
        TextBlock id = new()
        {
            Text = hex.Length >= 12 ? $"omni:{hex[..6]}…{hex[^4..]}" : person.OmniId,
            FontFamily = new Microsoft.UI.Xaml.Media.FontFamily("Consolas"),
            Style = (Style)Application.Current.Resources["SettingCaptionTextStyle"],
            IsTextSelectionEnabled = true,
        };
        ToolTipService.SetToolTip(id, person.OmniId);
        text.Children.Add(id);

        FrameworkElement action;
        if (person.IsSelf)
        {
            action = new TextBlock { Text = "Это вы", VerticalAlignment = VerticalAlignment.Center, Style = (Style)Application.Current.Resources["SettingCaptionTextStyle"] };
        }
        else if (person.IsContact)
        {
            action = new TextBlock { Text = "В контактах", VerticalAlignment = VerticalAlignment.Center, Style = (Style)Application.Current.Resources["SettingCaptionTextStyle"] };
        }
        else
        {
            Button add = new() { Content = "Добавить", VerticalAlignment = VerticalAlignment.Center };
            Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(add, $"Добавить {person.DisplayName}");
            add.Click += async (_, _) => await RequestFoundPersonAsync(person, add);
            action = add;
        }

        Grid row = new() { ColumnSpacing = 12, Padding = new Thickness(0, 4, 0, 4) };
        row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        row.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        row.Children.Add(picture);
        Grid.SetColumn(text, 1);
        row.Children.Add(text);
        Grid.SetColumn(action, 2);
        row.Children.Add(action);
        return row;
    }

    private static BitmapImage? BuildPhoto(string? jpegB64)
    {
        if (string.IsNullOrEmpty(jpegB64))
        {
            return null;
        }

        try
        {
            byte[] bytes = Convert.FromBase64String(jpegB64);
            BitmapImage image = new() { DecodePixelWidth = 80 };
            using global::Windows.Storage.Streams.InMemoryRandomAccessStream stream = new();
            using (global::Windows.Storage.Streams.DataWriter writer = new(stream.GetOutputStreamAt(0)))
            {
                writer.WriteBytes(bytes);
                writer.StoreAsync().GetAwaiter().GetResult();
                writer.FlushAsync().GetAwaiter().GetResult();
                writer.DetachStream();
            }

            stream.Seek(0);
            image.SetSource(stream);
            return image;
        }
        catch
        {
            return null; // a photo that does not decode: the initials stay
        }
    }

    /// "Добавить" on a found row: the find-by-Omni-ID path, with the person's own name for the contact.
    private async Task RequestFoundPersonAsync(FoundByName person, Button button)
    {
        button.IsEnabled = false;
        SetAddByOmniIdStatus($"Отправляю заявку «{person.DisplayName}»…");
        try
        {
            await Task.Run(() => rustBridge.FindContactByOmniId(person.OmniId, person.DisplayName));
            button.Content = "Заявка отправлена";
            SetAddByOmniIdStatus($"«{person.DisplayName}» получит заявку, как только соединение поднимется, и подтвердит её сам.");
            RefreshContactsList(force: true, selectContactId: null);
        }
        catch (Exception ex)
        {
            button.IsEnabled = true;
            SetAddByOmniIdStatus($"Не удалось отправить заявку: {NormalizeUserError(ex)}. Возможно, у человека выключено «Находить меня по Omni-ID».");
        }
    }
}
