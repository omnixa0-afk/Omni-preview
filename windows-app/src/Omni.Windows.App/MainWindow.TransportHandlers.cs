using System.Text.Json;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Omni.Windows.App.Presentation;
using Omni.Windows.App.ViewModels;
using Windows.ApplicationModel.DataTransfer;

namespace Omni.Windows.App;

public sealed partial class MainWindow
{
    private void OnPingClicked(object sender, RoutedEventArgs e)
    {
        try
        {
            string response = rustBridge.Ping();
            DiagnosticsPingStatusTextBlock.Text = $"Ответ ядра: {response}";
        }
        catch (Exception ex)
        {
            DiagnosticsPingStatusTextBlock.Text = $"Ошибка вызова: {ex.Message}";
        }
    }

    private async void OnRetryBootClicked(object sender, RoutedEventArgs e)
        => await BootAsync();

    private async void OnDeleteLocalDataAndRetryBootClicked(object sender, RoutedEventArgs e)
    {
        if (isOnboardingBusy)
        {
            return;
        }

        ContentDialog dialog = new()
        {
            Title = "Удалить локальные данные?",
            Content = "Это удалит текущую локальную базу этого окна и создаст новую пустую базу. Локальные чаты, контакты и ключи на этом устройстве будут потеряны.",
            PrimaryButtonText = "Удалить и создать новую",
            CloseButtonText = "Отмена",
            XamlRoot = Content.XamlRoot,
        };

        if (await dialog.ShowAsync() != ContentDialogResult.Primary)
        {
            return;
        }

        try
        {
            isOnboardingBusy = true;
            UpdateControlStates();

            await Task.Run(() => rustBridge.ResetLocalData());
            await BootAsync();
        }
        catch (Exception ex)
        {
            ShowOnboardingStep(
                OnboardingStep.BootRecovery,
                $"Не удалось удалить локальные данные: {NormalizeUserError(ex)}");
        }
        finally
        {
            isOnboardingBusy = false;
            UpdateControlStates();
        }
    }
}
