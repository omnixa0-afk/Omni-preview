using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media.Imaging;

namespace Omni.Windows.App.Features.Shared;

/// <summary>
/// One photo, wherever the chat shows one (a bubble, the draft above the composer). It draws
/// <see cref="LocalPath"/> when that file exists — its plaintext from the core, since Omni's storage is
/// encrypted at rest (<see cref="Core.MediaFiles"/>) — else <see cref="PreviewBase64"/> (the offer's
/// small JPEG). Either is decoded at <see cref="DecodeWidth"/> px — the memory a picture costs here is
/// bounded by where it is shown, never by what the file claims (media-messages.md §1).
/// </summary>
public sealed partial class PhotoView : UserControl
{
    public static readonly DependencyProperty LocalPathProperty = DependencyProperty.Register(
        nameof(LocalPath), typeof(string), typeof(PhotoView), new PropertyMetadata(null, (d, _) => ((PhotoView)d).Reload()));

    public static readonly DependencyProperty PreviewBase64Property = DependencyProperty.Register(
        nameof(PreviewBase64), typeof(string), typeof(PhotoView), new PropertyMetadata(null, (d, _) => ((PhotoView)d).Reload()));

    public static readonly DependencyProperty DecodeWidthProperty = DependencyProperty.Register(
        nameof(DecodeWidth), typeof(int), typeof(PhotoView), new PropertyMetadata(640, (d, _) => ((PhotoView)d).Reload()));

    private string? shownKey;

    public PhotoView()
    {
        InitializeComponent();
    }

    public string? LocalPath
    {
        get => (string?)GetValue(LocalPathProperty);
        set => SetValue(LocalPathProperty, value);
    }

    public string? PreviewBase64
    {
        get => (string?)GetValue(PreviewBase64Property);
        set => SetValue(PreviewBase64Property, value);
    }

    public int DecodeWidth
    {
        get => (int)GetValue(DecodeWidthProperty);
        set => SetValue(DecodeWidthProperty, value);
    }

    public CornerRadius FrameCornerRadius
    {
        get => FramePart.CornerRadius;
        set => FramePart.CornerRadius = value;
    }

    private async void Reload()
    {
        string? path = LocalPath;
        bool useFile = !string.IsNullOrWhiteSpace(path) && File.Exists(path);
        string? key = useFile ? "file:" + path + ":" + DecodeWidth : PreviewBase64 is { Length: > 0 } preview ? "preview:" + preview.Length + ":" + preview.GetHashCode() : null;
        if (string.Equals(key, shownKey, StringComparison.Ordinal))
        {
            return;
        }

        shownKey = key;
        if (key is null)
        {
            ImagePart.Source = null;
            return;
        }

        try
        {
            BitmapImage image = new() { DecodePixelWidth = Math.Max(1, DecodeWidth) };
            byte[]? bytes = useFile
                ? await Task.Run(() => Core.MediaFiles.ReadAllBytes(path!))
                : Convert.FromBase64String(PreviewBase64!);
            if (bytes is null)
            {
                ImagePart.Source = null;
                return;
            }

            using (var stream = await Core.MediaFiles.ToStreamAsync(bytes))
            {
                await image.SetSourceAsync(stream);
            }

            if (string.Equals(key, shownKey, StringComparison.Ordinal))
            {
                ImagePart.Source = image;
            }
        }
        catch (Exception ex) when (ex is FormatException or IOException or UnauthorizedAccessException or System.Runtime.InteropServices.COMException)
        {
            // A broken file or preview draws the empty frame; the bubble's caption still says what it is.
            ImagePart.Source = null;
        }
    }
}
