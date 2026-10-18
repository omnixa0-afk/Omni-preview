using System.Collections.ObjectModel;

namespace Omni.Windows.App.Features.Chats;

internal static class ChatThreadCollectionPatcher
{
    internal static void Patch(
        ObservableCollection<ChatThreadListItem> target,
        IReadOnlyList<ChatThreadListItem> sourceItems)
    {
        Dictionary<string, ChatThreadListItem> existingByDialogKey = target
            .ToDictionary(item => item.DialogKey, StringComparer.Ordinal);
        HashSet<string> nextDialogKeys = sourceItems
            .Select(item => item.DialogKey)
            .ToHashSet(StringComparer.Ordinal);

        for (int index = target.Count - 1; index >= 0; index--)
        {
            if (nextDialogKeys.Contains(target[index].DialogKey))
            {
                continue;
            }

            target.RemoveAt(index);
        }

        for (int index = 0; index < sourceItems.Count; index++)
        {
            ChatThreadListItem source = sourceItems[index];
            if (existingByDialogKey.TryGetValue(source.DialogKey, out ChatThreadListItem? existing))
            {
                existing.ApplyFrom(source);

                int existingIndex = target.IndexOf(existing);
                if (existingIndex >= 0 && existingIndex != index)
                {
                    target.Move(existingIndex, index);
                }

                continue;
            }

            target.Insert(index, source);
        }
    }
}
