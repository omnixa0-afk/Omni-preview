using System;
using Microsoft.UI;
using Microsoft.UI.Xaml.Data;
using Microsoft.UI.Xaml.Media;
using Omni.Windows.App.Presentation;

namespace Omni.Windows.App.Features.Chats;

// Resolves a chat bubble's fill from the message direction at render time.
// Lives here (not on ChatMessageListItem) so that item stays a headless-constructable POCO:
// WinRT Brush objects can't be created off the UI thread / in unit tests, but a converter only
// runs while XAML renders on the UI thread.
//
// Outgoing = OmniOutgoingBubble, incoming = OmniIncomingBubble, resolved for the WINDOW's theme
// (MainWindowPresentation.ResolveThemeBrush). Text colours don't depend on the direction and come
// straight from {ThemeResource} in the templates. A converter's result does not follow a theme
// switch by itself: MainWindow re-templates the bubble lists when the theme changes.
internal sealed class ChatBubbleBrushConverter : IValueConverter
{
    public object Convert(object value, Type targetType, object parameter, string language)
        => value is true
            ? MainWindowPresentation.ResolveThemeBrush("OmniOutgoingBubbleBrush", new SolidColorBrush(Colors.SteelBlue))
            : MainWindowPresentation.ResolveThemeBrush("OmniIncomingBubbleBrush", new SolidColorBrush(Colors.DimGray));

    public object ConvertBack(object value, Type targetType, object parameter, string language)
        => throw new NotSupportedException();
}
