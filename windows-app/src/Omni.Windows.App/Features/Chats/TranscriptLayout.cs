using Omni.Windows.App.Presentation;

namespace Omni.Windows.App.Features.Chats;

/// What a 1:1 transcript item shows depending on its neighbours (owner, 2026-10-10):
/// - a day header («Сегодня», «Вчера», «8 октября») over the first message of each day, so the
///   bubble itself shows only the time;
/// - the end of a run from one side, where the peer's avatar sits (incoming) and the gap widens —
///   as in Telegram.
/// Both belong to a message rather than being list items of their own: the transcript's windowing
/// and scroll anchors key on message items, and a separate row would be one more thing to skip.
internal static class TranscriptLayout
{
    /// Recompute every item from its neighbours (items are in time order). Cheap: the transcript is
    /// a bounded window, and an unchanged value raises no change notification.
    public static void Apply(IReadOnlyList<ChatMessageListItem> items, DateTime today, ChatTranscriptPeer? peer)
    {
        DateTime? previousDay = null;
        for (int i = 0; i < items.Count; i++)
        {
            ChatMessageListItem item = items[i];
            DateTime? day = DayOf(item);
            item.SetDayHeader(day is null || day == previousDay ? null : MainWindowPresentation.FormatDayHeader(item.CreatedAtUnixMs, today));
            previousDay = day ?? previousDay;

            ChatMessageListItem? next = i + 1 < items.Count ? items[i + 1] : null;
            bool runTail = next is null || next.IsOutgoing != item.IsOutgoing || DayOf(next) != day;
            item.SetLayout(runTail, peer);
        }
    }

    private static DateTime? DayOf(ChatMessageListItem item)
        => item.CreatedAtUnixMs <= 0
            ? null
            : DateTimeOffset.FromUnixTimeMilliseconds(item.CreatedAtUnixMs).ToLocalTime().Date;
}
