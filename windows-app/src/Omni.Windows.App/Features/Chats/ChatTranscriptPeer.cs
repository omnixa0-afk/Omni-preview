using System.ComponentModel;
using System.Runtime.CompilerServices;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Media;

namespace Omni.Windows.App.Features.Chats;

/// The peer of the open 1:1 dialog as the transcript draws it: one instance shared by all message
/// items, so a new avatar or name reaches every incoming row without touching them one by one.
internal sealed class ChatTranscriptPeer : INotifyPropertyChanged
{
    private string avatarText = "?";
    private ImageSource? avatarImage;
    private string avatarSeed = string.Empty;

    public event PropertyChangedEventHandler? PropertyChanged;

    public string AvatarText
    {
        get => avatarText;
        private set => SetField(ref avatarText, value);
    }

    public ImageSource? AvatarImage
    {
        get => avatarImage;
        private set
        {
            if (SetField(ref avatarImage, value))
            {
                OnPropertyChanged(nameof(AvatarImageVisibility));
                OnPropertyChanged(nameof(AvatarFallbackVisibility));
            }
        }
    }

    // UI thread only (binding); the brush is shared per tone and follows the theme (AvatarPalette).
    public Brush AvatarBrush => Omni.Windows.App.Presentation.AvatarPalette.BrushFor(avatarSeed);

    public Visibility AvatarImageVisibility => avatarImage is null ? Visibility.Collapsed : Visibility.Visible;

    public Visibility AvatarFallbackVisibility => avatarImage is null ? Visibility.Visible : Visibility.Collapsed;

    internal void Set(string text, ImageSource? image, string colorSeed)
    {
        AvatarText = string.IsNullOrWhiteSpace(text) ? "?" : text;
        AvatarImage = image;
        if (!string.Equals(avatarSeed, colorSeed, StringComparison.Ordinal))
        {
            avatarSeed = colorSeed;
            OnPropertyChanged(nameof(AvatarBrush));
        }
    }

    private void OnPropertyChanged([CallerMemberName] string? propertyName = null)
        => PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(propertyName));

    private bool SetField<T>(ref T field, T value, [CallerMemberName] string? propertyName = null)
    {
        if (EqualityComparer<T>.Default.Equals(field, value))
        {
            return false;
        }

        field = value;
        OnPropertyChanged(propertyName);
        return true;
    }
}
