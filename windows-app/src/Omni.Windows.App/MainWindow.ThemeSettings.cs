using System;
using System.Text.Json.Nodes;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Presentation;

namespace Omni.Windows.App;

public sealed partial class MainWindow
{
    private bool suppressThemeSelection;

    // Appearance theme is a per-device UI preference persisted in device-preferences.json (key "theme"),
    // reusing the same read-modify-write plumbing as the camera picker (MainWindow.Calls.cs). "system"
    // follows Windows (ElementTheme.Default); "light"/"dark" force the theme on the window root so every
    // {ThemeResource} brush re-resolves. Applied at startup and on selection change.
    // See docs/bugs/live-test-audit-2026-07-18.md (#2a).
    private void InitializeThemePicker()
    {
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
