using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media.Imaging;

namespace Omni.Windows.App.Features.Shared;

/// <summary>
/// The app's one full-window picture viewer (media-messages.md §2): every place that shows a photo
/// or an avatar opens it with <see cref="Show"/>. The picture is decoded no larger than the window,
/// whatever the file is. «Сохранить как…» (the right-click menu) asks the host for a place
/// (<see cref="SaveRequested"/>).
/// </summary>
public sealed partial class ImageViewer : UserControl
{
    private string? shownPath;
    private string? suggestedName;

    public ImageViewer()
    {
        InitializeComponent();
        KeyboardAccelerators.Add(CreateEscape());
    }

    /// (file path, suggested file name) — the host shows the save dialog and copies the file.
    public event Action<string, string>? SaveRequested;

    public bool IsOpen => Visibility == Visibility.Visible;

    public string? ShownPath => shownPath;

    /// <summary>Whether a picture is drawn (the test channel's `viewer.open` asks).</summary>
    internal bool HasPicture => ImagePart.Source is not null;

    public void Show(string path, string caption, string fileName)
    {
        if (!File.Exists(path))
        {
            return;
        }

        shownPath = path;
        suggestedName = fileName;
        CaptionPart.Text = caption;
        double scale = XamlRoot?.RasterizationScale ?? 1d;
        double windowWidth = XamlRoot?.Size.Width is > 0 and var width ? width : 1920d;
        ImagePart.Source = null;
        ZoomPart.ChangeView(0, 0, 1f, disableAnimation: true);
        Visibility = Visibility.Visible;
        CloseButtonPart.Focus(FocusState.Programmatic);
        LoadPicture(path, (int)Math.Ceiling(windowWidth * scale));
    }

    // From the plaintext the core hands over: Omni's storage is encrypted at rest (Core.MediaFiles).
    private async void LoadPicture(string path, int decodeWidth)
    {
        byte[]? bytes = await Task.Run(() => Core.MediaFiles.ReadAllBytes(path));
        if (bytes is null || !string.Equals(shownPath, path, StringComparison.Ordinal))
        {
            return;
        }

        try
        {
            BitmapImage image = new() { DecodePixelWidth = Math.Max(1, decodeWidth) };
            using (var stream = await Core.MediaFiles.ToStreamAsync(bytes))
            {
                await image.SetSourceAsync(stream);
            }

            if (string.Equals(shownPath, path, StringComparison.Ordinal))
            {
                ImagePart.Source = image;
            }
        }
        catch (Exception ex) when (ex is System.Runtime.InteropServices.COMException or ArgumentException)
        {
            // Not a picture this decoder reads: the viewer stays empty, the caption says what it was.
        }
    }

    public void Close()
    {
        Visibility = Visibility.Collapsed;
        ImagePart.Source = null;
        shownPath = null;
    }

    private KeyboardAccelerator CreateEscape()
    {
        KeyboardAccelerator escape = new() { Key = global::Windows.System.VirtualKey.Escape };
        escape.Invoked += (_, args) =>
        {
            if (IsOpen)
            {
                Close();
                args.Handled = true;
            }
        };
        return escape;
    }

    // At zoom 1 the picture fits the window (in a ScrollViewer it would take its own pixel size).
    private void OnZoomSizeChanged(object sender, SizeChangedEventArgs e)
    {
        FitPart.Width = e.NewSize.Width;
        FitPart.Height = e.NewSize.Height;
    }

    private void OnBackdropTapped(object sender, TappedRoutedEventArgs e) => Close();

    // A tap on the picture itself is not a tap beside it.
    private void OnImageTapped(object sender, TappedRoutedEventArgs e) => e.Handled = true;

    private void OnCloseClicked(object sender, RoutedEventArgs e) => Close();

    private async void OnCopyClicked(object sender, RoutedEventArgs e)
    {
        if (shownPath is null)
        {
            return;
        }

        try
        {
            string path = shownPath;
            byte[]? bytes = await Task.Run(() => Core.MediaFiles.ReadAllBytes(path));
            if (bytes is null)
            {
                return;
            }

            var stream = await Core.MediaFiles.ToStreamAsync(bytes);
            var package = new global::Windows.ApplicationModel.DataTransfer.DataPackage();
            package.SetBitmap(global::Windows.Storage.Streams.RandomAccessStreamReference.CreateFromStream(stream));
            global::Windows.ApplicationModel.DataTransfer.Clipboard.SetContent(package);
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException or System.Runtime.InteropServices.COMException)
        {
            // Nothing to copy from: the file went away while the viewer was open.
        }
    }

    private void OnSaveClicked(object sender, RoutedEventArgs e)
    {
        if (shownPath is not null)
        {
            SaveRequested?.Invoke(shownPath, suggestedName ?? Path.GetFileName(shownPath));
        }
    }
}
