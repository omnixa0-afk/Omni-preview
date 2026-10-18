using Omni.Windows.App.Core;

namespace Omni.Windows.App.Features.Chats;

internal readonly record struct TranscriptWindowBoundary(long? CreatedAtUnixMs, string? MessageId);

internal static class TranscriptWindowing
{
    internal static int FindInsertIndex(
        IReadOnlyList<ChatMessageListItem> items,
        ChatMessageRecord message)
    {
        for (int index = 0; index < items.Count; index++)
        {
            ChatMessageRecord? current = items[index].SourceMessage;
            if (current is null)
            {
                continue;
            }

            if (CompareMessageOrder(message, current) < 0)
            {
                return index;
            }
        }

        return items.Count;
    }

    internal static bool IsMessageOlderThanWindow(
        ChatMessageRecord message,
        long? oldestLoadedCreatedAtUnixMs,
        string? oldestLoadedMessageId)
    {
        if (!oldestLoadedCreatedAtUnixMs.HasValue ||
            string.IsNullOrWhiteSpace(oldestLoadedMessageId))
        {
            return false;
        }

        int createdAtComparison = message.CreatedAtUnixMs.CompareTo(oldestLoadedCreatedAtUnixMs.Value);
        return createdAtComparison < 0 ||
               (createdAtComparison == 0 &&
                string.CompareOrdinal(message.Id, oldestLoadedMessageId) < 0);
    }

    internal static TranscriptWindowBoundary FindOldestBoundary(
        IReadOnlyList<ChatMessageListItem> items,
        long? fallbackCreatedAtUnixMs = null,
        string? fallbackMessageId = null)
    {
        if (items.Count > 0 &&
            items[0].SourceMessage is ChatMessageRecord oldestMessage)
        {
            return new TranscriptWindowBoundary(oldestMessage.CreatedAtUnixMs, oldestMessage.Id);
        }

        return new TranscriptWindowBoundary(fallbackCreatedAtUnixMs, fallbackMessageId);
    }

    internal static int CalculateTrimCount(
        int itemCount,
        bool isNearBottom,
        bool isLoadingOlderMessages,
        bool isRestoringAnchor,
        int hardItemLimit,
        int softItemLimit)
    {
        if (itemCount <= hardItemLimit ||
            !isNearBottom ||
            isLoadingOlderMessages ||
            isRestoringAnchor)
        {
            return 0;
        }

        return Math.Max(0, itemCount - softItemLimit);
    }

    private static int CompareMessageOrder(ChatMessageRecord left, ChatMessageRecord right)
    {
        int createdAtComparison = left.CreatedAtUnixMs.CompareTo(right.CreatedAtUnixMs);
        return createdAtComparison != 0
            ? createdAtComparison
            : string.CompareOrdinal(left.Id, right.Id);
    }
}
