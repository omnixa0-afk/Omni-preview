using System.Collections.ObjectModel;
using Microsoft.UI.Xaml;
using Omni.Windows.App.Features.Chats;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class ChatThreadCollectionPatcherTests
{
    [Fact]
    public void Patch_UpdatesMovesInsertsAndRemovesByDialogKey()
    {
        ChatThreadListItem existingA = CreateItem("dialog:a", "A", "old-a", 10);
        ChatThreadListItem existingB = CreateItem("dialog:b", "B", "old-b", 20);
        ChatThreadListItem removed = CreateItem("dialog:removed", "Removed", "old-removed", 5);
        ObservableCollection<ChatThreadListItem> target = [existingA, existingB, removed];

        ChatThreadListItem nextB = CreateItem("dialog:b", "B2", "new-b", 30);
        ChatThreadListItem insertedC = CreateItem("dialog:c", "C", "new-c", 25);

        ChatThreadCollectionPatcher.Patch(target, [nextB, insertedC, existingA]);

        Assert.Equal(["dialog:b", "dialog:c", "dialog:a"], target.Select(item => item.DialogKey));
        Assert.Same(existingB, target[0]);
        Assert.Same(insertedC, target[1]);
        Assert.Same(existingA, target[2]);
        Assert.Equal("B2", existingB.Title);
        Assert.Equal("new-b", existingB.Signature);
        Assert.DoesNotContain(removed, target);
    }

    private static ChatThreadListItem CreateItem(
        string dialogKey,
        string title,
        string signature,
        long sortUnixMs)
    {
        ChatThreadListItem item = new(dialogKey);
        item.ApplyFrom(
            nextContactId: $"{dialogKey}:contact",
            nextAvatarText: title[..1],
            nextAvatarImage: null,
            nextAvatarImageVisibility: Visibility.Collapsed,
            nextAvatarFallbackVisibility: Visibility.Visible,
            nextAvatarLoadingVisibility: Visibility.Collapsed,
            nextTitle: title,
            nextPreviewText: $"{title} preview",
            nextStatusText: "status",
            nextTimeText: "time",
            nextSortUnixMs: sortUnixMs,
            nextSignature: signature);
        return item;
    }
}
