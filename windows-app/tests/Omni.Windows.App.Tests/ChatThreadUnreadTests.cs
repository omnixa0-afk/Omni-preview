using System.Text.Json;
using Microsoft.UI.Xaml;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Chats;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class ChatThreadUnreadTests
{
    [Fact]
    public void The_pill_shows_the_count_past_two_digits_as_99_plus_and_hides_at_zero()
    {
        ChatThreadListItem item = new("dialog:a");
        Assert.Equal(Visibility.Collapsed, item.UnreadBadgeVisibility);

        item.ApplyFrom(null, "A", null, Visibility.Collapsed, Visibility.Visible, Visibility.Collapsed,
            "A", "hi", string.Empty, "12:00", 1, "sig", nextUnreadCount: 3);
        Assert.Equal(Visibility.Visible, item.UnreadBadgeVisibility);
        Assert.Equal("3", item.UnreadBadgeText);
        Assert.Equal("99+", ChatThreadListItem.FormatUnreadCount(140));

        item.ClearUnread();
        Assert.Equal(Visibility.Collapsed, item.UnreadBadgeVisibility);
    }

    [Fact]
    public void A_group_summary_reads_its_unread_count_and_an_old_core_reads_as_none()
    {
        const string withCount = """{"group_id":"g","name":"G","dialog_key":"group:g","my_role":"member","my_status":"member","member_count":2,"active_member_count":2,"epoch":1,"last_message_preview":"","last_activity_unix_ms":0,"unread_count":4}""";
        const string without = """{"group_id":"g","name":"G","dialog_key":"group:g","my_role":"member","my_status":"member","member_count":2,"active_member_count":2,"epoch":1,"last_message_preview":"","last_activity_unix_ms":0}""";

        Assert.Equal(4, JsonSerializer.Deserialize(withCount, OmniJsonSerializerContext.Default.GroupSummary)!.UnreadCount);
        Assert.Equal(0, JsonSerializer.Deserialize(without, OmniJsonSerializerContext.Default.GroupSummary)!.UnreadCount);
    }
}
