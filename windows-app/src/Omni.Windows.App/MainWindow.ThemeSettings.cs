using System;
using System.Text.Json.Nodes;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Presentation;

namespace Omni.Windows.App;

public sealed partial class MainWindow
{
    private const string RecordsVideoPreferenceKey = "recordsVideo";

    /// What the composer's record button makes: kept on this device, like the theme.
    private void WireRecordKindPreference()
    {
        try
        {
            Features.Shared.MessageComposer.RecordsVideo =
                ReadDevicePreferencesDocument()[RecordsVideoPreferenceKey]?.GetValue<bool>() == true;
        }
        catch (Exception ex) when (ex is InvalidOperationException or FormatException)
        {
            // A value someone else wrote there: start with the microphone.
            Features.Shared.MessageComposer.RecordsVideo = false;
        }

        Features.Shared.MessageComposer.SaveRecordsVideo = value =>
        {
            JsonObject document = ReadDevicePreferencesDocument();
            document[RecordsVideoPreferenceKey] = value;
            WriteDevicePreferencesDocument(document);
        };
    }

    private bool suppressThemeSelection;

    // Appearance theme is a per-device UI preference persisted in device-preferences.json (key "theme"),
    // reusing the same read-modify-write plumbing as the camera picker (MainWindow.Calls.cs). "system"
    // follows Windows (ElementTheme.Default); "light"/"dark" force the theme on the window root so every
    // {ThemeResource} brush re-resolves. Applied at startup and on selection change.
    // See docs/bugs/live-test-audit-2026-07-18.md (#2a).
    private void InitializeThemePicker()
    {
        InitializeListColumnWidth();
        InitializeEmojiStylePicker();
        string stored = LoadPersistedThemeKey();
        ApplyElementTheme(ThemeKeyToElementTheme(stored));

        suppressThemeSelection = true;
        try
        {
            SettingsThemeComboBox.SelectedItem = FindThemeComboItem(stored);
            if (SettingsThemeComboBox.SelectedItem is null && SettingsThemeComboBox.Items.Count > 0)
            {
                SettingsThemeComboBox.SelectedIndex = 0;
            }
        }
        finally
        {
            suppressThemeSelection = false;
        }
    }

    private void InitializeEmojiStylePicker()
    {
        ApplyEmojiFamily();
        Features.Shared.EmojiFonts.Changed += ApplyEmojiFamily;
        if (!Features.Shared.EmojiFonts.NotoDrawnHere &&
            SettingsEmojiStyleComboBox.Items.OfType<ComboBoxItem>().FirstOrDefault(i => (string)i.Tag == "noto") is { } noto)
        {
            SettingsEmojiStyleComboBox.Items.Remove(noto);
        }

        SettingsEmojiStyleComboBox.SelectedItem = SettingsEmojiStyleComboBox.Items
            .OfType<ComboBoxItem>()
            .FirstOrDefault(i => (string)i.Tag == Features.Shared.EmojiFonts.Current);
        SettingsEmojiPreviewTextBlock.FontFamily = Features.Shared.EmojiFonts.Family;
        SettingsEmojiStyleComboBox.SelectionChanged += (_, _) =>
        {
            if (SettingsEmojiStyleComboBox.SelectedItem is not ComboBoxItem { Tag: string style } || style == Features.Shared.EmojiFonts.Current)
            {
                return;
            }

            Features.Shared.EmojiFonts.Set(style);
            JsonObject document = ReadDevicePreferencesDocument();
            document[Features.Shared.EmojiFonts.PreferenceKey] = style;
            WriteDevicePreferencesDocument(document);
        };
    }

    // The sections hand the family down to their text; the feed's rows are drawn anew for their chips.
    private void ApplyEmojiFamily()
    {
        Microsoft.UI.Xaml.Media.FontFamily family = Features.Shared.EmojiFonts.Family;
        ChatsSectionHost.FontFamily = family;
        WorkspacesSectionHost.FontFamily = family;
        ContactsSectionHost.FontFamily = family;
        SettingsEmojiPreviewTextBlock.FontFamily = family;
        MessageInputTextBox.FontFamily = family;
        RebuildItemTemplate(MessagesListView);
        GroupConversation.RebuildMessageBubbles();
    }

    private void OnSettingsThemeSelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (suppressThemeSelection)
        {
            return;
        }

        string key = (SettingsThemeComboBox.SelectedItem as ComboBoxItem)?.Tag as string ?? "system";
        ApplyElementTheme(ThemeKeyToElementTheme(key));
        SavePersistedThemeKey(key);
    }

    private ComboBoxItem? FindThemeComboItem(string themeKey)
    {
        foreach (object item in SettingsThemeComboBox.Items)
        {
            if (item is ComboBoxItem candidate &&
                string.Equals(candidate.Tag as string, themeKey, StringComparison.OrdinalIgnoreCase))
            {
                return candidate;
            }
        }

        return null;
    }

    private bool rootThemeHooked;

    private void ApplyElementTheme(ElementTheme theme)
    {
        if (Content is FrameworkElement root)
        {
            if (!rootThemeHooked)
            {
                // "Системная" follows Windows live: the window's theme changes without us setting it.
                root.ActualThemeChanged += (_, _) => OnWindowThemeChanged(root.ActualTheme);
                rootThemeHooked = true;
            }

            root.RequestedTheme = theme;
            OnWindowThemeChanged(root.ActualTheme);
        }
    }

    private void OnWindowThemeChanged(ElementTheme actualTheme)
    {
        if (MainWindowPresentation.ActiveTheme == actualTheme)
        {
            return;
        }

        MainWindowPresentation.ActiveTheme = actualTheme;
        AvatarPalette.ApplyTheme(darkTheme: actualTheme != ElementTheme.Light);
        // The caption bar is drawn by Windows and follows the WINDOWS theme, not ours: a light
        // window kept a dark caption (seen live 2026-10-10).
        AppWindow.TitleBar.PreferredTheme = actualTheme == ElementTheme.Light
            ? Microsoft.UI.Windowing.TitleBarTheme.Light
            : Microsoft.UI.Windowing.TitleBarTheme.Dark;
        // Bubble fills come from a converter, whose result doesn't follow a theme switch: rebuild
        // the bubbles so it runs again under the new theme.
        RebuildItemTemplate(MessagesListView);
        GroupConversation.RebuildMessageBubbles();
    }

    /// What opening a conversation cost until the window had nothing more urgent to do than idle work:
    /// its rows built and laid out, which the handlers' own timing does not see.
    internal static void TraceOpened(Microsoft.UI.Dispatching.DispatcherQueue queue, string what, long started)
        => queue.TryEnqueue(Microsoft.UI.Dispatching.DispatcherQueuePriority.Low, () =>
            global::Omni.Windows.App.Core.AppTrace.Write("ui/opened", $"{what} {System.Diagnostics.Stopwatch.GetElapsedTime(started).TotalMilliseconds:F0}ms"));

    internal static void RebuildItemTemplate(ItemsControl list)
    {
        DataTemplate template = list.ItemTemplate;
        list.ItemTemplate = null;
        list.ItemTemplate = template;
    }

    private static ElementTheme ThemeKeyToElementTheme(string? themeKey)
        => (themeKey ?? string.Empty).Trim().ToLowerInvariant() switch
        {
            "light" => ElementTheme.Light,
            "dark" => ElementTheme.Dark,
            _ => ElementTheme.Default,
        };

    // The list column's width, the person's, one for every section (Features/Shared/SectionRail.cs): a
    // device preference too (key "listWidth"), kept when the edge is let go.
    private void InitializeListColumnWidth()
    {
        try
        {
            if (ReadDevicePreferencesDocument()["listWidth"]?.GetValue<double>() is double kept)
            {
                Features.Shared.SectionRail.Restore(kept);
            }
        }
        catch
        {
            // Not a number: the default width.
        }

        Features.Shared.SectionRail.Chosen += width =>
        {
            JsonObject document = ReadDevicePreferencesDocument();
            document["listWidth"] = width;
            WriteDevicePreferencesDocument(document);
        };

        // The other columns the person can size: each under its own key.
        foreach (Features.Shared.ColumnWidth column in Features.Shared.ColumnWidth.All)
        {
            try
            {
                if (ReadDevicePreferencesDocument()[column.PreferenceKey]?.GetValue<double>() is double kept)
                {
                    column.Restore(kept);
                }
            }
            catch
            {
                // Not a number: the default width.
            }

            column.Chosen += width =>
            {
                JsonObject document = ReadDevicePreferencesDocument();
                document[column.PreferenceKey] = width;
                WriteDevicePreferencesDocument(document);
            };
        }
    }

    private string LoadPersistedThemeKey()
    {
        try
        {
            JsonObject document = ReadDevicePreferencesDocument();
            if (document["theme"]?.GetValue<string>() is string stored && !string.IsNullOrWhiteSpace(stored))
            {
                return stored;
            }
        }
        catch
        {
            // Corrupt/unexpected value: fall back to following the system theme.
        }

        return "system";
    }

    private void SavePersistedThemeKey(string themeKey)
    {
        JsonObject document = ReadDevicePreferencesDocument();
        document["theme"] = themeKey;
        WriteDevicePreferencesDocument(document);
    }
}
