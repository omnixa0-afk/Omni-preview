using System;
using System.IO;
using System.Runtime.InteropServices.WindowsRuntime;
using System.Threading.Tasks;
using Microsoft.UI;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Media.Imaging;
using Microsoft.UI.Xaml.Shapes;
using Windows.Foundation;
using Windows.Graphics.Imaging;
using Windows.Storage;
using Windows.Storage.Streams;

namespace Omni.Windows.App.Features.Shared;

/// <summary>
/// «Фото профиля» / «Фото группы» before it is taken: the picture under a round frame, moved by the
/// mouse, zoomed by the wheel or the slider, turned by a quarter (owner, 2026-10-30: «не даёт его
/// обрезать/повернуть»). The result is a square PNG of what the frame shows; the core makes the avatar
/// out of it as out of any picked file (resize to its size, JPEG), so its centre crop no longer decides
/// what the contacts see.
/// </summary>
internal sealed class PhotoCropDialog
{
    // The frame on screen, in DIPs.
    private const double Frame = 320;
    // The working copy: enough for the closest zoom of the frame, small enough to move smoothly.
    private const int WorkingLongSide = 2048;
    private const double MaxZoom = 5;
    // A picture past this is no photo a person picks; WIC would still decode it whole.
    private const long MaxSourcePixels = 120_000_000;

    /// The open dialog, for the UI test channel (there is at most one: dialogs do not stack).
    internal static PhotoCropDialog? Open { get; private set; }

    private readonly byte[] pixels;
    private readonly int width;
    private readonly int height;
    private readonly Image image = new() { Stretch = Stretch.Fill };
    private readonly MatrixTransform placement = new();
    private readonly Slider zoomSlider;
    private ContentDialog? dialog;
    private bool accepted;
    private bool settingSlider;
    private uint? dragPointer;
    private Point dragFrom;

    // Quarter turns clockwise, the zoom (DIPs per working pixel) and the point of the turned picture
    // under the frame's centre.
    private int turns;
    private double scale;
    private double centreX;
    private double centreY;

    private PhotoCropDialog(byte[] pixels, int width, int height)
    {
        this.pixels = pixels;
        this.width = width;
        this.height = height;
        scale = CoverScale;
        centreX = width / 2.0;
        centreY = height / 2.0;
        zoomSlider = new Slider
        {
            Minimum = 1,
            Maximum = MaxZoom,
            StepFrequency = 0.01,
            Value = 1,
            VerticalAlignment = VerticalAlignment.Center,
        };
        ToolTipService.SetToolTip(zoomSlider, "Масштаб");
        zoomSlider.ValueChanged += (_, e) =>
        {
            if (!settingSlider)
            {
                ZoomAround(new Point(Frame / 2, Frame / 2), e.NewValue * CoverScale);
            }
        };
    }

    /// Shows the picture at <paramref name="sourcePath"/> under the frame and writes the framed square
    /// as a PNG to <paramref name="outputPath"/>. False when the person cancelled.
    internal static async Task<bool> ShowAsync(XamlRoot root, ElementTheme theme, string title, string sourcePath, string outputPath)
    {
        (byte[] pixels, int width, int height) = await LoadAsync(sourcePath);
        PhotoCropDialog crop = new(pixels, width, height);
        await crop.SetImageAsync();
        crop.dialog = new ContentDialog
        {
            Title = title,
            Content = crop.Build(),
            PrimaryButtonText = "Готово",
            CloseButtonText = "Отмена",
            DefaultButton = ContentDialogButton.Primary,
            XamlRoot = root,
            RequestedTheme = theme,
        };
        crop.dialog.PrimaryButtonClick += (_, _) => crop.accepted = true;
        Open = crop;
        try
        {
            await crop.dialog.ShowAsync();
        }
        finally
        {
            Open = null;
        }

        if (!crop.accepted)
        {
            return false;
        }

        await crop.SaveAsync(outputPath);
        return true;
    }

    /// The test channel's hand: turn, zoom (1 = the picture just covers the frame) and press «Готово».
    internal void ApplyForUiTest(int quarterTurns, double zoom)
    {
        for (int i = 0; i < quarterTurns; i++)
        {
            Turn();
        }

        ZoomAround(new Point(Frame / 2, Frame / 2), Math.Clamp(zoom, 1, MaxZoom) * CoverScale);
    }

    internal void AcceptForUiTest()
    {
        accepted = true;
        dialog?.Hide();
    }

    internal string DescribeForUiTest() =>
        $"ok source={width}x{height} turns={turns} zoom={scale / CoverScale:0.00} centre={centreX:0},{centreY:0}";

    private bool Sideways => turns % 2 == 1;

    private double TurnedWidth => Sideways ? height : width;

    private double TurnedHeight => Sideways ? width : height;

    // The zoom at which the picture's short side just fills the frame: the frame never shows past it.
    private double CoverScale => Frame / Math.Min(width, height);

    private FrameworkElement Build()
    {
        Canvas canvas = new() { Width = Frame, Height = Frame };
        image.Width = width;
        image.Height = height;
        image.RenderTransform = placement;
        canvas.Children.Add(image);

        // Outside the circle is dimmed: the avatar is round everywhere it is shown.
        GeometryGroup outside = new() { FillRule = FillRule.EvenOdd };
        outside.Children.Add(new RectangleGeometry { Rect = new Rect(0, 0, Frame, Frame) });
        outside.Children.Add(new EllipseGeometry { Center = new Point(Frame / 2, Frame / 2), RadiusX = Frame / 2, RadiusY = Frame / 2 });
        Grid frame = new()
        {
            Width = Frame,
            Height = Frame,
            // Transparent, not null: the empty parts of the frame take the drag too.
            Background = new SolidColorBrush(Colors.Transparent),
            Clip = new RectangleGeometry { Rect = new Rect(0, 0, Frame, Frame) },
            HorizontalAlignment = HorizontalAlignment.Center,
        };
        frame.Children.Add(canvas);
        frame.Children.Add(new Microsoft.UI.Xaml.Shapes.Path
        {
            Data = outside,
            Fill = new SolidColorBrush(ColorHelper.FromArgb(0x99, 0, 0, 0)),
            IsHitTestVisible = false,
        });
        frame.Children.Add(new Ellipse
        {
            Stroke = new SolidColorBrush(ColorHelper.FromArgb(0xB0, 0xFF, 0xFF, 0xFF)),
            StrokeThickness = 1.5,
            IsHitTestVisible = false,
        });
        frame.PointerPressed += OnPointerPressed;
        frame.PointerMoved += OnPointerMoved;
        frame.PointerReleased += OnPointerEnded;
        frame.PointerCaptureLost += OnPointerEnded;
        frame.PointerWheelChanged += OnPointerWheelChanged;

        Button turn = new()
        {
            Content = new FontIcon { Glyph = "\uE7AD", FontSize = 16 },
            // Not the shared button's minimum width: an icon button, as wide as its icon.
            MinWidth = 0,
            Padding = new Thickness(10, 8, 10, 8),
            VerticalAlignment = VerticalAlignment.Center,
        };
        ToolTipService.SetToolTip(turn, "Повернуть");
        Microsoft.UI.Xaml.Automation.AutomationProperties.SetName(turn, "Повернуть");
        turn.Click += (_, _) => Turn();

        Grid controls = new() { ColumnSpacing = 10, Width = Frame, HorizontalAlignment = HorizontalAlignment.Center };
        controls.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        controls.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        controls.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        controls.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        Brush muted = Presentation.MainWindowPresentation.ResolveThemeBrush("OmniMutedTextBrush", new SolidColorBrush(Colors.Gray));
        FontIcon smaller = new() { Glyph = "\uE71F", FontSize = 14, Foreground = muted, VerticalAlignment = VerticalAlignment.Center };
        FontIcon larger = new() { Glyph = "\uE8A3", FontSize = 14, Foreground = muted, VerticalAlignment = VerticalAlignment.Center };
        Grid.SetColumn(turn, 0);
        Grid.SetColumn(smaller, 1);
        Grid.SetColumn(zoomSlider, 2);
        Grid.SetColumn(larger, 3);
        controls.Children.Add(turn);
        controls.Children.Add(smaller);
        controls.Children.Add(zoomSlider);
        controls.Children.Add(larger);

        StackPanel content = new() { Spacing = 12 };
        content.Children.Add(frame);
        content.Children.Add(controls);
        Place();
        return content;
    }

    private void OnPointerPressed(object sender, PointerRoutedEventArgs e)
    {
        UIElement frame = (UIElement)sender;
        if (frame.CapturePointer(e.Pointer))
        {
            dragPointer = e.Pointer.PointerId;
            dragFrom = e.GetCurrentPoint(frame).Position;
            e.Handled = true;
        }
    }

    private void OnPointerMoved(object sender, PointerRoutedEventArgs e)
    {
        if (dragPointer != e.Pointer.PointerId)
        {
            return;
        }

        Point at = e.GetCurrentPoint((UIElement)sender).Position;
        centreX -= (at.X - dragFrom.X) / scale;
        centreY -= (at.Y - dragFrom.Y) / scale;
        dragFrom = at;
        Place();
        e.Handled = true;
    }

    private void OnPointerEnded(object sender, PointerRoutedEventArgs e)
    {
        if (dragPointer == e.Pointer.PointerId)
        {
            dragPointer = null;
            ((UIElement)sender).ReleasePointerCapture(e.Pointer);
        }
    }

    private void OnPointerWheelChanged(object sender, PointerRoutedEventArgs e)
    {
        Microsoft.UI.Input.PointerPoint point = e.GetCurrentPoint((UIElement)sender);
        double steps = point.Properties.MouseWheelDelta / 120.0;
        ZoomAround(point.Position, scale * Math.Pow(1.15, steps));
        e.Handled = true;
    }

    /// Zooms keeping the picture's point under <paramref name="at"/> (frame DIPs) where it is.
    private void ZoomAround(Point at, double newScale)
    {
        newScale = Math.Clamp(newScale, CoverScale, CoverScale * MaxZoom);
        double pointX = centreX + (at.X - Frame / 2) / scale;
        double pointY = centreY + (at.Y - Frame / 2) / scale;
        scale = newScale;
        centreX = pointX - (at.X - Frame / 2) / scale;
        centreY = pointY - (at.Y - Frame / 2) / scale;
        Place();
    }

    /// A quarter turn clockwise; the point under the frame's centre stays under it.
    private void Turn()
    {
        (centreX, centreY) = (TurnedHeight - centreY, centreX);
        turns = (turns + 1) % 4;
        Place();
    }

    private void Place()
    {
        double halfSpan = Frame / 2 / scale;
        centreX = Math.Clamp(centreX, halfSpan, Math.Max(halfSpan, TurnedWidth - halfSpan));
        centreY = Math.Clamp(centreY, halfSpan, Math.Max(halfSpan, TurnedHeight - halfSpan));

        // Working pixel → turned picture → frame: x' = x·M11 + y·M21 + OffsetX, y' = x·M12 + y·M22 + OffsetY.
        (double m11, double m12, double m21, double m22, double offsetX, double offsetY) = turns switch
        {
            1 => (0, 1, -1, 0, height, 0),
            2 => (-1, 0, 0, -1, width, height),
            3 => (0, -1, 1, 0, 0, width),
            _ => (1, 0, 0, 1, 0, 0),
        };
        placement.Matrix = new Matrix(
            m11 * scale,
            m12 * scale,
            m21 * scale,
            m22 * scale,
            offsetX * scale + Frame / 2 - centreX * scale,
            offsetY * scale + Frame / 2 - centreY * scale);

        settingSlider = true;
        zoomSlider.Value = scale / CoverScale;
        settingSlider = false;
    }

    private async Task SetImageAsync()
    {
        SoftwareBitmap bitmap = SoftwareBitmap.CreateCopyFromBuffer(
            pixels.AsBuffer(), BitmapPixelFormat.Bgra8, width, height, BitmapAlphaMode.Premultiplied);
        SoftwareBitmapSource source = new();
        await source.SetBitmapAsync(bitmap);
        image.Source = source;
    }

    /// The picture as the camera meant it (EXIF turn applied), its long side at most WorkingLongSide.
    private static async Task<(byte[] Pixels, int Width, int Height)> LoadAsync(string sourcePath)
    {
        StorageFile file = await StorageFile.GetFileFromPathAsync(sourcePath);
        using IRandomAccessStreamWithContentType stream = await file.OpenReadAsync();
        BitmapDecoder decoder;
        try
        {
            decoder = await BitmapDecoder.CreateAsync(stream);
        }
        catch (Exception)
        {
            throw new InvalidOperationException("Это не изображение, которое Windows умеет открыть.");
        }

        if ((long)decoder.PixelWidth * decoder.PixelHeight > MaxSourcePixels)
        {
            throw new InvalidOperationException("Изображение слишком большое.");
        }

        double shrink = Math.Min(1.0, (double)WorkingLongSide / Math.Max(decoder.PixelWidth, decoder.PixelHeight));
        uint scaledWidth = Math.Max(1u, (uint)Math.Round(decoder.PixelWidth * shrink));
        uint scaledHeight = Math.Max(1u, (uint)Math.Round(decoder.PixelHeight * shrink));
        PixelDataProvider data = await decoder.GetPixelDataAsync(
            BitmapPixelFormat.Bgra8,
            BitmapAlphaMode.Premultiplied,
            new BitmapTransform { ScaledWidth = scaledWidth, ScaledHeight = scaledHeight, InterpolationMode = BitmapInterpolationMode.Fant },
            ExifOrientationMode.RespectExifOrientation,
            ColorManagementMode.ColorManageToSRgb);
        byte[] pixels = data.DetachPixelData();
        // The scale is of the stored frame; an EXIF quarter turn swaps the sides of what comes out.
        bool swapped = decoder.OrientedPixelWidth != decoder.PixelWidth;
        int width = (int)(swapped ? scaledHeight : scaledWidth);
        int height = (int)(swapped ? scaledWidth : scaledHeight);
        if (pixels.Length != width * height * 4)
        {
            throw new InvalidOperationException("Не удалось прочитать изображение.");
        }

        return (pixels, width, height);
    }

    /// What the frame shows, as a square of as many pixels as the working copy has across it: sampled
    /// one to one, never down (that would alias) — the core shrinks it to the avatar with a proper filter.
    private async Task SaveAsync(string outputPath)
    {
        int side = Math.Clamp((int)Math.Round(Frame / scale), 64, WorkingLongSide);
        byte[] square = await Task.Run(() => Sample(pixels, width, height, turns, scale, centreX, centreY, side));
        Directory.CreateDirectory(System.IO.Path.GetDirectoryName(outputPath)!);
        using FileStream file = new(outputPath, FileMode.Create, FileAccess.ReadWrite);
        BitmapEncoder encoder = await BitmapEncoder.CreateAsync(BitmapEncoder.PngEncoderId, file.AsRandomAccessStream());
        encoder.SetPixelData(BitmapPixelFormat.Bgra8, BitmapAlphaMode.Premultiplied, (uint)side, (uint)side, 96, 96, square);
        await encoder.FlushAsync();
    }

    internal static byte[] Sample(byte[] pixels, int width, int height, int turns, double scale, double centreX, double centreY, int side)
    {
        byte[] square = new byte[side * side * 4];
        for (int v = 0; v < side; v++)
        {
            for (int u = 0; u < side; u++)
            {
                // Output pixel → turned picture → working pixel (the inverse of Place's matrix).
                double turnedX = centreX + ((u + 0.5) * Frame / side - Frame / 2) / scale;
                double turnedY = centreY + ((v + 0.5) * Frame / side - Frame / 2) / scale;
                (double x, double y) = turns switch
                {
                    1 => (turnedY, height - turnedX),
                    2 => (width - turnedX, height - turnedY),
                    3 => (width - turnedY, turnedX),
                    _ => (turnedX, turnedY),
                };
                Bilinear(pixels, width, height, x - 0.5, y - 0.5, square.AsSpan((v * side + u) * 4, 4));
            }
        }

        return square;
    }

    private static void Bilinear(byte[] pixels, int width, int height, double x, double y, Span<byte> into)
    {
        x = Math.Clamp(x, 0, width - 1);
        y = Math.Clamp(y, 0, height - 1);
        int x0 = (int)x;
        int y0 = (int)y;
        int x1 = Math.Min(x0 + 1, width - 1);
        int y1 = Math.Min(y0 + 1, height - 1);
        double fx = x - x0;
        double fy = y - y0;
        for (int channel = 0; channel < 4; channel++)
        {
            double top = pixels[(y0 * width + x0) * 4 + channel] * (1 - fx) + pixels[(y0 * width + x1) * 4 + channel] * fx;
            double bottom = pixels[(y1 * width + x0) * 4 + channel] * (1 - fx) + pixels[(y1 * width + x1) * 4 + channel] * fx;
            into[channel] = (byte)Math.Round(top * (1 - fy) + bottom * fy);
        }
    }
}
