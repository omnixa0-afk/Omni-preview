using System.ComponentModel;
using System.Runtime.CompilerServices;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Media.Imaging;

namespace Omni.Windows.App.Features.Chats;

internal sealed class ChatThreadListItem : INotifyPropertyChanged
{
    private string? contactId;
    private string avatarText = string.Empty;
    private BitmapImage? avatarImage;
    private Visibility avatarImageVisibility;
    private Visibility avatarFallbackVisibility;
    private Visibility avatarLoadingVisibility;
    private string title = string.Empty;
    private string? avatarSeed;
    private string previewText = string.Empty;
    private string statusText = string.Empty;
    private string timeText = string.Empty;
    private long sortUnixMs;
    private string signature = string.Empty;
    // Presence dot on the dialog avatar (mirrors the contact list / chat header). Brush is left
    // null by default — never eagerly construct a WinRT brush here, the item is built headless in
    // tests (a SolidColorBrush ctor off the UI thread throws COMException).
    private Visibility presenceDotVisibility;
    private Brush? presenceDotBrush;
    private int unreadCount;

    public ChatThreadListItem(string dialogKey, string? groupId = null, bool isGroupInvite = false)
    {
        DialogKey = dialogKey;
        GroupId = groupId;
        IsGroupInvite = isGroupInvite;
    }

    // A group row (a group we're in, or an invitation to one) — the chat list holds both kinds;
    // the kind is fixed per row key (a group's dialog key / "group-invite:<id>"), so it is set
    // once here and never patched.
    public string? GroupId { get; }
    public bool IsGroupInvite { get; }
    public bool IsGroup => GroupId is not null;

    public event PropertyChangedEventHandler? PropertyChanged;

    public string? ContactId
    {
        get => contactId;
        private set => SetField(ref contactId, value);
    }

    public string DialogKey { get; }

    public string AvatarText
    {
        get => avatarText;
        private set => SetField(ref avatarText, value);
    }

    public BitmapImage? AvatarImage
    {
        get => avatarImage;
        private set => SetField(ref avatarImage, value);
    }

    public Visibility AvatarImageVisibility
    {
        get => avatarImageVisibility;
        private set => SetField(ref avatarImageVisibility, value);
    }

    public Visibility AvatarFallbackVisibility
    {
        get => avatarFallbackVisibility;
        private set => SetField(ref avatarFallbackVisibility, value);
    }

    public Visibility AvatarLoadingVisibility
    {
        get => avatarLoadingVisibility;
        private set => SetField(ref avatarLoadingVisibility, value);
    }

    public string Title
    {
        get => title;
        private set => SetField(ref title, value);
    }

    /// What the photo-less avatar's colour is picked by (AvatarPalette): the contact's account id,
    /// a group's id. Falls back to the row key.
    public string AvatarSeed => avatarSeed ?? GroupId ?? DialogKey;

    private void SetAvatarSeed(string? value)
    {
        if (string.Equals(avatarSeed, value, StringComparison.Ordinal))
        {
            return;
        }

        avatarSeed = value;
        OnPropertyChanged(nameof(AvatarSeed));
        OnPropertyChanged(nameof(AvatarBrush));
    }

    // Read by the binding on the UI thread only (a brush is a WinRT object; tests build rows headless).
    public Brush AvatarBrush => Omni.Windows.App.Presentation.AvatarPalette.BrushFor(AvatarSeed);

    /// Groups wear a "people" mark before the name, so a group never reads as a person.
    public Visibility GroupMarkVisibility => IsGroup ? Visibility.Visible : Visibility.Collapsed;

    public string PreviewText
    {
        get => previewText;
        private set => SetField(ref previewText, value);
    }

    public string StatusText
    {
        get => statusText;
        private set
        {
            SetField(ref statusText, value);
            OnPropertyChanged(nameof(CornerText));
        }
    }

    /// The shared row's corner word (App.xaml, PersonRowTemplate): the call / delivery word.
    public string CornerText => statusText;

    public string TimeText
    {
        get => timeText;
        private set => SetField(ref timeText, value);
    }

    public long SortUnixMs
    {
        get => sortUnixMs;
        private set => SetField(ref sortUnixMs, value);
    }

    public string Signature
    {
        get => signature;
        private set => SetField(ref signature, value);
    }

    public Visibility PresenceDotVisibility
    {
        get => presenceDotVisibility;
        private set => SetField(ref presenceDotVisibility, value);
    }

    public Brush? PresenceDotBrush
    {
        get => presenceDotBrush;
        private set => SetField(ref presenceDotBrush, value);
    }

    /// Incoming lines this device has not read (the core's read marks).
    public int UnreadCount
    {
        get => unreadCount;
        private set
        {
            if (unreadCount == value)
            {
                return;
            }

            unreadCount = value;
            OnPropertyChanged();
            OnPropertyChanged(nameof(UnreadBadgeVisibility));
            OnPropertyChanged(nameof(UnreadBadgeText));
        }
    }

    /// The chat is on screen: its pill goes now, before the core's next count.
    internal void ClearUnread() => UnreadCount = 0;

    public Visibility UnreadBadgeVisibility => unreadCount > 0 ? Visibility.Visible : Visibility.Collapsed;

    public string UnreadBadgeText => FormatUnreadCount(unreadCount);

    /// «99+» past two digits: the pill stays one size.
    internal static string FormatUnreadCount(int count) => count > 99 ? "99+" : count.ToString(System.Globalization.CultureInfo.InvariantCulture);

    internal void ApplyFrom(ChatThreadListItem source)
    {
        if (!string.Equals(DialogKey, source.DialogKey, StringComparison.Ordinal))
        {
            throw new InvalidOperationException(
                $"Cannot apply dialog '{source.DialogKey}' to thread item '{DialogKey}'.");
        }

        ApplyFrom(
            source.ContactId,
            source.AvatarText,
            source.AvatarImage,
            source.AvatarImageVisibility,
            source.AvatarFallbackVisibility,
            source.AvatarLoadingVisibility,
            source.Title,
            source.PreviewText,
            source.StatusText,
            source.TimeText,
            source.SortUnixMs,
            source.Signature,
            source.PresenceDotVisibility,
            source.PresenceDotBrush,
            source.avatarSeed,
            source.UnreadCount);
    }

    internal void ApplyFrom(
        string? nextContactId,
        string nextAvatarText,
        BitmapImage? nextAvatarImage,
        Visibility nextAvatarImageVisibility,
        Visibility nextAvatarFallbackVisibility,
        Visibility nextAvatarLoadingVisibility,
        string nextTitle,
        string nextPreviewText,
        string nextStatusText,
        string nextTimeText,
        long nextSortUnixMs,
        string nextSignature,
        Visibility nextPresenceDotVisibility = Visibility.Collapsed,
        Brush? nextPresenceDotBrush = null,
        string? nextAvatarSeed = null,
        int nextUnreadCount = 0)
    {
        ContactId = nextContactId;
        AvatarText = nextAvatarText;
        AvatarImage = nextAvatarImage;
        AvatarImageVisibility = nextAvatarImageVisibility;
        AvatarFallbackVisibility = nextAvatarFallbackVisibility;
        AvatarLoadingVisibility = nextAvatarLoadingVisibility;
        Title = nextTitle;
        SetAvatarSeed(nextAvatarSeed);
        PreviewText = nextPreviewText;
        StatusText = nextStatusText;
        TimeText = nextTimeText;
        SortUnixMs = nextSortUnixMs;
        Signature = nextSignature;
        PresenceDotVisibility = nextPresenceDotVisibility;
        PresenceDotBrush = nextPresenceDotBrush;
        UnreadCount = nextUnreadCount;
    }

    private void OnPropertyChanged([CallerMemberName] string? propertyName = null)
        => PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(propertyName));

    private void SetField<T>(ref T field, T value, [CallerMemberName] string? propertyName = null)
    {
        if (EqualityComparer<T>.Default.Equals(field, value))
        {
            return;
        }

        field = value;
        OnPropertyChanged(propertyName);
    }
}
