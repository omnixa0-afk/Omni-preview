using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Chats;
using Omni.Windows.App.Presentation;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class TranscriptLayoutTests
{
    private static readonly DateTime Today = new(2026, 10, 10);

    [Fact]
    public void Only_the_first_message_of_each_day_carries_the_header()
    {
        List<ChatMessageListItem> items =
        [
            Item("a", new DateTime(2026, 10, 7, 0, 39, 0)),
            Item("b", new DateTime(2026, 10, 7, 12, 39, 0)),
            Item("c", new DateTime(2026, 10, 9, 23, 59, 0)),
            Item("d", new DateTime(2026, 10, 10, 16, 33, 0)),
            Item("e", new DateTime(2026, 10, 10, 16, 40, 0)),
        ];

        TranscriptLayout.Apply(items, Today, null);

        Assert.Equal(["7 октября", "", "Вчера", "Сегодня", ""], items.Select(i => i.DayHeaderText));
        Assert.Equal(Microsoft.UI.Xaml.Visibility.Collapsed, items[1].DayHeaderVisibility);
    }

    [Fact]
    public void A_header_moves_when_an_older_page_is_prepended()
    {
        List<ChatMessageListItem> items = [Item("d", new DateTime(2026, 10, 8, 9, 0, 0))];
        TranscriptLayout.Apply(items, Today, null);
        Assert.Equal("8 октября", items[0].DayHeaderText);

        items.Insert(0, Item("c", new DateTime(2026, 10, 8, 8, 0, 0)));
        TranscriptLayout.Apply(items, Today, null);

        Assert.Equal(["8 октября", ""], items.Select(i => i.DayHeaderText));
    }

    [Fact]
    public void A_run_ends_where_the_side_or_the_day_changes()
    {
        List<ChatMessageListItem> items =
        [
            Item("a", new DateTime(2026, 10, 9, 10, 0, 0)),
            Item("b", new DateTime(2026, 10, 10, 10, 0, 0)),
            Item("c", new DateTime(2026, 10, 10, 10, 1, 0)),
            Item("d", new DateTime(2026, 10, 10, 10, 2, 0), "outgoing"),
            Item("e", new DateTime(2026, 10, 10, 10, 3, 0), "outgoing"),
        ];

        TranscriptLayout.Apply(items, Today, null);

        Assert.Equal([true, false, true, false, true], items.Select(i => i.IsRunTail));
        Assert.Equal([1d, 0d, 1d, 0d, 1d], items.Select(i => i.AvatarOpacity));
        Assert.Equal(Microsoft.UI.Xaml.Visibility.Collapsed, items[3].AvatarSlotVisibility);
    }

    [Fact]
    public void Another_year_names_the_year()
        => Assert.Equal(
            "31 декабря 2025",
            MainWindowPresentation.FormatDayHeader(UnixMs(new DateTime(2025, 12, 31, 10, 0, 0)), Today));

    private static ChatMessageListItem Item(string id, DateTime local, string direction = "incoming")
    {
        ChatMessageListItem item = new(id);
        item.ApplyFrom(
            new ChatMessageRecord(
                Id: id,
                PeerKey: "dialog:instance:peer-1",
                Direction: direction,
                Body: id,
                Status: "delivered",
                RemoteDeliveredAtUnixMs: null,
                CreatedAtUnixMs: UnixMs(local),
                UpdatedAtUnixMs: UnixMs(local),
                Attachment: null),
            null,
            null,
            null);
        return item;
    }

    private static long UnixMs(DateTime local)
        => new DateTimeOffset(local, TimeZoneInfo.Local.GetUtcOffset(local)).ToUnixTimeMilliseconds();
}
