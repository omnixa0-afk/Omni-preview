using System;
using System.Runtime.InteropServices;
using Microsoft.UI;
using Microsoft.UI.Text;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Windows.Graphics;

namespace Omni.Windows.App.Features.Calls;

/// <summary>
/// The owner's bar for the whole of a remote-control session (docs/decisions/remote-access-for-contacts.md):
/// "«Имя» управляет этим компьютером · [Остановить]", above every window, top centre of the primary
/// display. It cannot be closed or hidden while the session runs — only "Остановить" (or the session
/// ending) takes it away. Visible to the controller too, on purpose: nothing about the session is
/// hidden from either side. UI thread only.
/// </summary>
internal sealed class RemoteControlBar
{
    private const int WidthDip = 460;
    private const int HeightDip = 46;
    private const int TopMarginDip = 6;

    private Window? window;
    private TextBlock? message;
    private string? callId;
    private bool allowClose;

    /// "Остановить" was pressed for this call id.
    public event Action<string>? StopRequested;

    public void Show(string text, string sessionCallId)
    {
        callId = sessionCallId;
        if (window is null)
        {
            Create();
        }

        message!.Text = text;
        if (!window!.AppWindow.IsVisible)
        {
            PlaceTopCentre(window.AppWindow);
            window.AppWindow.Show(false); // never steal focus from what the owner is doing
        }
    }

    public bool IsVisible => window?.AppWindow.IsVisible == true;

    public void Hide()
    {
        if (window?.AppWindow.IsVisible == true)
        {
            window.AppWindow.Hide();
        }
    }

    public void Close()
    {
        allowClose = true;
        window?.Close();
        window = null;
    }

    private void Create()
    {
        message = new TextBlock
        {
            VerticalAlignment = VerticalAlignment.Center,
            FontSize = 14,
            FontWeight = FontWeights.SemiBold,
            Foreground = new SolidColorBrush(Colors.White),
            TextTrimming = TextTrimming.CharacterEllipsis,
        };
        var stop = new Button
        {
            Content = "Остановить",
            VerticalAlignment = VerticalAlignment.Center,
            Padding = new Thickness(14, 4, 14, 4),
            Background = new SolidColorBrush(Colors.White),
            Foreground = new SolidColorBrush(ColorHelper.FromArgb(0xFF, 0xB3, 0x26, 0x1E)),
            CornerRadius = new CornerRadius(14),
        };
        stop.Click += (_, _) =>
        {
            if (callId is { } id)
            {
                StopRequested?.Invoke(id);
            }
        };
        var icon = new FontIcon
        {
            Glyph = "",
            FontSize = 16,
            Foreground = new SolidColorBrush(Colors.White),
            VerticalAlignment = VerticalAlignment.Center,
        };

        var grid = new Grid
        {
            Background = new SolidColorBrush(ColorHelper.FromArgb(0xFF, 0xB3, 0x26, 0x1E)),
            Padding = new Thickness(14, 0, 8, 0),
            ColumnSpacing = 10,
            RequestedTheme = ElementTheme.Dark,
        };
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        Grid.SetColumn(message, 1);
        Grid.SetColumn(stop, 2);
        grid.Children.Add(icon);
        grid.Children.Add(message);
        grid.Children.Add(stop);

        window = new Window { Content = grid, Title = "Omni — удалённое управление" };
        AppWindow appWindow = window.AppWindow;
        if (appWindow.Presenter is OverlappedPresenter presenter)
        {
            presenter.IsAlwaysOnTop = true;
            presenter.IsResizable = false;
            presenter.IsMaximizable = false;
            presenter.IsMinimizable = false;
            presenter.SetBorderAndTitleBar(true, false);
        }

        appWindow.IsShownInSwitchers = false;
        appWindow.Closing += (_, args) =>
        {
            if (!allowClose)
            {
                args.Cancel = true; // Alt+F4 does not hide the fact that someone controls this computer
            }
        };
    }

    private void PlaceTopCentre(AppWindow appWindow)
    {
        DisplayArea area = DisplayArea.Primary;
        double scale = GetDpiForWindow(Win32Interop.GetWindowFromWindowId(appWindow.Id)) / 96.0;
        if (scale <= 0)
        {
            scale = 1;
        }

        int width = (int)(WidthDip * scale);
        int height = (int)(HeightDip * scale);
        RectInt32 work = area.WorkArea;
        appWindow.MoveAndResize(new RectInt32(
            work.X + (work.Width - width) / 2,
            work.Y + (int)(TopMarginDip * scale),
            width,
            height));
    }

    [DllImport("user32.dll")]
    private static extern uint GetDpiForWindow(IntPtr hwnd);
}
