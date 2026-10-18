using System;
using System.Text.Json.Nodes;
using System.Threading.Tasks;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Omni.Windows.App.ViewModels;

namespace Omni.Windows.App;

public sealed partial class MainWindow
{
    private bool suppressCongestionSelection;
    private bool congestionVisibilityWired;

    // Ручной выбор контроллера перегрузки — устройственная настройка в device-preferences.json
    // (ключ "congestionControl"), та же плита чтения-записи, что у темы и камеры.
    //
    // Применяется НЕ мгновенно к живым соединениям: quinn не умеет менять контроллер на
    // поднятом соединении (у `Connection` есть только окна), контроллер выбирается при создании
    // эндпоинта. Поэтому смена = сохранить + сообщить ядру + переустановить меш. Перезапуск
    // приложения не нужен, но связи переустановятся.
    //
    // Правило и оба замера под ним — в ядре (`select_congestion_control`),
    // docs/bugs/cubic-caps-this-path-at-four-percent.md.
    private void InitializeCongestionPicker()
    {
        string stored = LoadPersistedCongestionMode();
        ApplyCongestionModeToCore(stored);

        suppressCongestionSelection = true;
        try
        {
            SettingsCongestionComboBox.SelectedItem = FindCongestionComboItem(stored);
            if (SettingsCongestionComboBox.SelectedItem is null && SettingsCongestionComboBox.Items.Count > 0)
            {
                SettingsCongestionComboBox.SelectedIndex = 0;
            }
        }
        finally
        {
            suppressCongestionSelection = false;
        }

        // Строка состояния спрашивает ядро, а на старте окна оно часто ещё холодное — тот же
        // приём, что у списка узлов: перечитать при показе раздела.
        if (!congestionVisibilityWired)
        {
            SettingsSectionRoot.RegisterPropertyChangedCallback(
                Microsoft.UI.Xaml.UIElement.VisibilityProperty,
                OnSettingsVisibilityChangedForCongestion);
            congestionVisibilityWired = true;
        }

        RefreshCongestionStatusText(stored);
    }

    private void OnSettingsVisibilityChangedForCongestion(
        Microsoft.UI.Xaml.DependencyObject sender,
        Microsoft.UI.Xaml.DependencyProperty dp)
    {
        if (SettingsSectionRoot.Visibility != Microsoft.UI.Xaml.Visibility.Visible)
        {
            return;
        }

        DispatcherQueue.TryEnqueue(() => RefreshCongestionStatusText(LoadPersistedCongestionMode()));
    }

    private void OnSettingsCongestionSelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (suppressCongestionSelection)
        {
            return;
        }

        string mode = CongestionControlPreference.Normalize(
            (SettingsCongestionComboBox.SelectedItem as ComboBoxItem)?.Tag as string);

        SavePersistedCongestionMode(mode);
        ApplyCongestionModeToCore(mode);
        RefreshCongestionStatusText(mode);
        ApplyCongestionChangeToLiveLinks(mode);
    }

    private ComboBoxItem? FindCongestionComboItem(string mode)
    {
        foreach (object item in SettingsCongestionComboBox.Items)
        {
            if (item is ComboBoxItem candidate &&
                string.Equals(candidate.Tag as string, mode, StringComparison.OrdinalIgnoreCase))
            {
                return candidate;
            }
        }

        return null;
    }

    private void ApplyCongestionModeToCore(string mode)
    {
        try
        {
            rustBridge.SetCongestionControl(mode);
            AppTrace.Write("net/congestion", $"preference={mode}");
        }
        catch (Exception ex)
        {
            // Осечка здесь не должна мешать сети подняться: ядро останется на прежнем решении.
            AppTrace.WriteFailure("net/congestion", "set", ex);
        }
    }

    private void RefreshCongestionStatusText(string selected)
    {
        string effective;
        try
        {
            effective = rustBridge.ReadEffectiveCongestionControl();
        }
        catch (Exception ex)
        {
            AppTrace.WriteFailure("net/congestion", "read-effective", ex);
            SettingsCongestionStatusTextBlock.Text = string.Empty;
            return;
        }

        SettingsCongestionStatusTextBlock.Text =
            CongestionControlPreference.StatusText(selected, effective);
    }

    /// <summary>
    /// Применить выбор к живым линкам сейчас, а не после следующего перезапуска.
    ///
    /// Сначала здесь стоял перезапуск меша — и он оказался слишком дорогим: замер 2026-08-31
    /// показал `node_endpoints=none`, `link=searching` дольше 60 секунд, потому что перезапуск
    /// сносит не только сокеты, а ещё дискавери, анонсы, проброс порта и заставляет заново
    /// проходить рандеву. Для смены одной настройки это неприемлемо.
    ///
    /// Ядро вместо этого пересобирает конфиги интерфейсов и закрывает соединения, которые едут на
    /// прежнем контроллере (менять его на живом соединении quinn не умеет). Дискавери, анонсы и
    /// проброс порта переживают смену, и линк восстанавливается обычным передозвоном.
    ///
    /// Во время звонка НЕ трогаем: обрыв медиа ради смены диагностической настройки — поломка,
    /// а не размен. Тогда выбор доживёт до ближайшей переустановки связи.
    ///
    /// ИЗВЕСТНОЕ ОГРАНИЧЕНИЕ (2026-08-31): передача вложения такой защиты не имеет — оболочка не
    /// держит состояния «идёт передача». Обрыв её не теряет (у передачи есть `resume_offset`), но
    /// стоит паузы и повторного запроса. Появится состояние передач в UI — добавить сюда же.
    /// </summary>
    private void ApplyCongestionChangeToLiveLinks(string mode)
    {
        if (windowState.Onboarding.ShellMode != AppShellMode.Authenticated)
        {
            return;
        }

        if (currentCallInboxSnapshot?.CurrentCall is not null)
        {
            SettingsCongestionStatusTextBlock.Text =
                $"{SettingsCongestionStatusTextBlock.Text} · применится после звонка";
            return;
        }

        _ = Task.Run(() =>
        {
            string outcome;
            try
            {
                outcome = rustBridge.ApplyCongestionControlNow();
                AppTrace.Write("net/congestion", $"applied {mode}: {outcome}");
            }
            catch (Exception ex)
            {
                AppTrace.WriteFailure("net/congestion", "apply-now", ex);
                outcome = "error";
            }

            DispatcherQueue.TryEnqueue(() =>
            {
                RefreshCongestionStatusText(mode);
                // Ноль закрытых — это НЕ «применено к линку»: соединений просто не было. Молчать
                // об этом нельзя, иначе настройка выглядит подействовавшей там, где не к чему.
                if (outcome == "closed=0")
                {
                    SettingsCongestionStatusTextBlock.Text =
                        $"{SettingsCongestionStatusTextBlock.Text} · подействует на следующем соединении";
                }

                RefreshTransportStatus();
            });
        });
    }

    private string LoadPersistedCongestionMode()
    {
        try
        {
            JsonObject document = ReadDevicePreferencesDocument();
            if (document["congestionControl"]?.GetValue<string>() is string stored)
            {
                return CongestionControlPreference.Normalize(stored);
            }
        }
        catch
        {
            // Испорченное значение: возвращаемся к автоматике, а не запираем пользователя в
            // контроллере, которого он не выбирал.
        }

        return CongestionControlPreference.Auto;
    }

    private void SavePersistedCongestionMode(string mode)
    {
        JsonObject document = ReadDevicePreferencesDocument();
        document["congestionControl"] = mode;
        WriteDevicePreferencesDocument(document);
    }
}
