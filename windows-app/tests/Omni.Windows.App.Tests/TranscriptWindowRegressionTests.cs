using System.Text.RegularExpressions;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Chats;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class TranscriptWindowRegressionTests
{
    [Fact]
    public void ChatRefreshService_UsesPagedTranscriptPaths_AndFixedWindowLimits()
    {
        string source = File.ReadAllText(ResolveRepoPath("src", "Omni.Windows.App", "ChatRefreshService.cs"));

        Assert.Equal(80, ChatRefreshService.TranscriptPageSize);
        Assert.Equal(250, ChatRefreshService.TranscriptSoftItemLimit);
        Assert.Equal(400, ChatRefreshService.TranscriptHardItemLimit);

        Assert.Contains("Math.Clamp(", source, StringComparison.Ordinal);
        Assert.Contains("string.Equals(signature, lastRenderedSignature", source, StringComparison.Ordinal);
        Assert.Contains("rustBridge.ListMessagesPage(", source, StringComparison.Ordinal);
        Assert.DoesNotContain("rustBridge.ListMessages(", source, StringComparison.Ordinal);
        Assert.Contains("public MessageRefreshPayload? FetchLiveMessages(", source, StringComparison.Ordinal);
        Assert.Contains("public MessageRefreshPayload? FetchTailWindow(", source, StringComparison.Ordinal);
        Assert.DoesNotContain("public MessageRefreshPayload? FetchMessages(", source, StringComparison.Ordinal);
        Assert.Contains("FetchOlderMessagesPage", source, StringComparison.Ordinal);
    }

    [Fact]
    public void TranscriptSurface_UsesWindowState_AnchorRestore_AndTrimGuards()
    {
        string chatsSource = File.ReadAllText(ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.Chats.cs"));
        string eventsSource = File.ReadAllText(ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.SectionEvents.cs"));
        string chatsXamlSource = File.ReadAllText(ResolveRepoPath("src", "Omni.Windows.App", "Features", "Chats", "ChatsSectionControl.xaml"));

        // The chats surface is split across MainWindow.Chats partials; the transcript-window guards
        // below may live in any of them. PrepareTranscriptForTailRebase + ApplyOlderMessagesPage stay
        // in the main file (the substring isolation below relies on that), but the standalone
        // Contains/DoesNotContain checks search the combined partial source so a behaviour-preserving
        // file split doesn't trip the guard.
        string transcriptSource = File.ReadAllText(ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.Chats.Transcript.cs"));
        string headerSource = File.ReadAllText(ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.Chats.Header.cs"));
        string combinedChatsSource = string.Join("\n", chatsSource, transcriptSource, headerSource);
        int prepareTailRebaseStart = chatsSource.IndexOf(
            "private void PrepareTranscriptForTailRebase",
            StringComparison.Ordinal);
        int prepareTailRebaseEnd = chatsSource.IndexOf(
            "private void ApplyOlderMessagesPage",
            StringComparison.Ordinal);
        string prepareTailRebaseSource = chatsSource.Substring(
            prepareTailRebaseStart,
            prepareTailRebaseEnd - prepareTailRebaseStart);
        int sameDialogResetGuardIndex = prepareTailRebaseSource.IndexOf(
            "if (!sameDialog || clearVisibleTranscript)",
            StringComparison.Ordinal);
        int clearDialogIndex = prepareTailRebaseSource.IndexOf(
            "chatRefreshService.ClearDialog(dialogKey);",
            StringComparison.Ordinal);
        int liveCursorResetIndex = prepareTailRebaseSource.IndexOf(
            "activeChatTranscriptState.LiveCursor = null;",
            StringComparison.Ordinal);

        Assert.Contains("TranscriptTopLoadThresholdPx = 32d", combinedChatsSource, StringComparison.Ordinal);
        Assert.Contains("MessagesNearBottomThresholdPx = 80d", combinedChatsSource, StringComparison.Ordinal);
        Assert.Contains("ServerHasOlderMessages", combinedChatsSource, StringComparison.Ordinal);
        Assert.Contains("HasTrimmedOlderMessages", combinedChatsSource, StringComparison.Ordinal);
        Assert.Contains("LoadOlderMessagesPage()", combinedChatsSource, StringComparison.Ordinal);
        Assert.Contains("ApplyOlderMessagesPage", combinedChatsSource, StringComparison.Ordinal);
        Assert.Contains("RestoreTranscriptScrollAnchor", combinedChatsSource, StringComparison.Ordinal);
        Assert.Contains("ScrollTranscriptToBottom", combinedChatsSource, StringComparison.Ordinal);
        Assert.DoesNotContain("MessagesListView.ScrollIntoView(latest)", combinedChatsSource, StringComparison.Ordinal);
        Assert.Contains("!currentChatMessageItemsById.ContainsKey(message.Id)", combinedChatsSource, StringComparison.Ordinal);
        Assert.Contains("messageItems.Move(existingIndex, index);", combinedChatsSource, StringComparison.Ordinal);
        Assert.Contains("currentChatMessagesById.Remove(messageId);", combinedChatsSource, StringComparison.Ordinal);
        Assert.Contains("currentChatSecurityDetailsByMessageId.Remove(messageId);", combinedChatsSource, StringComparison.Ordinal);
        Assert.Contains("activeChatTranscriptState.HasTrimmedOlderMessages = true;", combinedChatsSource, StringComparison.Ordinal);
        Assert.Contains("IsMessageOlderThanCurrentTranscriptWindow", combinedChatsSource, StringComparison.Ordinal);
        Assert.Contains("if (transcriptOlderLoadCancellation is null)", combinedChatsSource, StringComparison.Ordinal);
        Assert.Contains("if (!restoreQueued)", combinedChatsSource, StringComparison.Ordinal);
        Assert.DoesNotContain("ClearCurrentChatMessageCache();\r\n        messageItems.Clear();", combinedChatsSource, StringComparison.Ordinal);
        Assert.True(prepareTailRebaseStart >= 0 && prepareTailRebaseEnd > prepareTailRebaseStart, "Expected to isolate PrepareTranscriptForTailRebase source.");
        Assert.True(sameDialogResetGuardIndex >= 0, "Expected same-dialog reset guard for tail rebase.");
        Assert.True(clearDialogIndex > sameDialogResetGuardIndex, "Expected dialog cache clear to stay inside the guarded hard-reset path.");
        Assert.True(liveCursorResetIndex > sameDialogResetGuardIndex, "Expected LiveCursor reset to stay inside the guarded hard-reset path.");
        Assert.Contains("ShowsScrollingPlaceholders=\"False\"", chatsXamlSource, StringComparison.Ordinal);
        Assert.Contains("ItemsUpdatingScrollMode=\"KeepScrollOffset\"", chatsXamlSource, StringComparison.Ordinal);
        Assert.Contains("<TransitionCollection />", chatsXamlSource, StringComparison.Ordinal);
        // The list row's corner word lives in the shared row (App.xaml, PersonRowTemplate); the
        // transcript binds no status text of its own.
        Assert.Empty(Regex.Matches(chatsXamlSource, "Text=\"\\{Binding StatusText\\}\"").Cast<Match>());
        Assert.Contains("ItemTemplate=\"{StaticResource PersonRowTemplate}\"", chatsXamlSource, StringComparison.Ordinal);
        Assert.Contains("ToolTipService.ToolTip=\"{Binding DeliveryIndicatorToolTipText}\"", chatsXamlSource, StringComparison.Ordinal);
        Assert.Contains("AutomationProperties.Name=\"{Binding DeliveryIndicatorAutomationName}\"", chatsXamlSource, StringComparison.Ordinal);
        Assert.Contains("Glyph=\"{Binding DeliveryPrimaryGlyph}\"", chatsXamlSource, StringComparison.Ordinal);
        Assert.Contains("FontFamily=\"{ThemeResource SymbolThemeFontFamily}\"", chatsXamlSource, StringComparison.Ordinal);
        Assert.Contains("MaxWidth=\"{Binding BubbleMaxWidth}\"", chatsXamlSource, StringComparison.Ordinal);
        Assert.Contains("MessagesHostBorder.SizeChanged += OnMessagesHostBorderSizeChanged;", eventsSource, StringComparison.Ordinal);
        Assert.Matches(new Regex("Text=\"\\{Binding Body\\}\"\\s+TextWrapping=\"Wrap\"", RegexOptions.CultureInvariant), chatsXamlSource);
        Assert.Matches(new Regex("Text=\"\\{Binding AttachmentFileName\\}\"\\s+TextWrapping=\"Wrap\"", RegexOptions.CultureInvariant), chatsXamlSource);
        Assert.Matches(new Regex("Text=\"\\{Binding AttachmentStatusText\\}\"\\s+TextWrapping=\"Wrap\"", RegexOptions.CultureInvariant), chatsXamlSource);
        Assert.Contains("MessagesListView.Loaded += OnMessagesListViewLoaded;", eventsSource, StringComparison.Ordinal);
    }

    [Fact]
    public void BackgroundRefreshPipeline_TracksTranscriptLoadVersion_AndDoesNotUseWholeHistoryPath()
    {
        string refreshModelsSource = File.ReadAllText(ResolveRepoPath("src", "Omni.Windows.App", "RefreshModels.cs"));
        string backgroundSource = File.ReadAllText(ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.BackgroundRefresh.cs"));

        Assert.Contains("TranscriptLoadVersion", refreshModelsSource, StringComparison.Ordinal);
        Assert.Contains("CurrentTranscriptItemCount", refreshModelsSource, StringComparison.Ordinal);
        Assert.Contains("CurrentTranscriptOldestLoadedCreatedAtUnixMs", refreshModelsSource, StringComparison.Ordinal);
        Assert.Contains("CurrentTranscriptOldestLoadedMessageId", refreshModelsSource, StringComparison.Ordinal);

        Assert.Contains("activeChatTranscriptState.LoadVersion", backgroundSource, StringComparison.Ordinal);
        Assert.Contains("request.TranscriptLoadVersion", backgroundSource, StringComparison.Ordinal);
        Assert.Contains("chatRefreshService.FetchLiveMessages(", backgroundSource, StringComparison.Ordinal);
        Assert.DoesNotContain("chatRefreshService.FetchMessages(", backgroundSource, StringComparison.Ordinal);
        Assert.DoesNotContain("ListMessages(", backgroundSource, StringComparison.Ordinal);
    }

    [Fact]
    public void ActiveTranscriptCallSites_UseExplicitLiveAndTailPaths()
    {
        string chatsSource = File.ReadAllText(ResolveRepoPath("src", "Omni.Windows.App", "MainWindow.Chats.cs"));

        Assert.Contains("chatRefreshService.FetchLiveMessages(", chatsSource, StringComparison.Ordinal);
        Assert.Contains("chatRefreshService.FetchTailWindow(", chatsSource, StringComparison.Ordinal);
        Assert.DoesNotContain("chatRefreshService.FetchMessages(", chatsSource, StringComparison.Ordinal);
    }

    [Fact]
    public void TranscriptWindowing_KeepsOrderingBoundaryAndTrimRulesPure()
    {
        ChatMessageRecord existingMessage = CreateMessage("message-b", 20);
        ChatMessageRecord olderMessage = CreateMessage("message-a", 10);
        ChatMessageListItem existingItem = new(existingMessage.Id);
        existingItem.ApplyFrom(existingMessage, null, null, null);
        List<ChatMessageListItem> items = [existingItem];

        Assert.Equal(0, TranscriptWindowing.FindInsertIndex(items, olderMessage));
        Assert.True(TranscriptWindowing.IsMessageOlderThanWindow(olderMessage, 20, "message-b"));

        TranscriptWindowBoundary boundary = TranscriptWindowing.FindOldestBoundary(items, 1, "fallback");
        Assert.Equal(20, boundary.CreatedAtUnixMs);
        Assert.Equal("message-b", boundary.MessageId);

        Assert.Equal(151, TranscriptWindowing.CalculateTrimCount(401, true, false, false, 400, 250));
        Assert.Equal(0, TranscriptWindowing.CalculateTrimCount(401, false, false, false, 400, 250));
    }

    private static string ResolveRepoPath(params string[] relativeSegments)
    {
        DirectoryInfo? current = new(AppContext.BaseDirectory);
        while (current is not null &&
               !string.Equals(current.Name, "windows-app", StringComparison.OrdinalIgnoreCase))
        {
            current = current.Parent;
        }

        Assert.NotNull(current);

        string[] segments =
        [
            current!.FullName,
            .. relativeSegments,
        ];

        return Path.GetFullPath(Path.Combine(segments));
    }

    private static ChatMessageRecord CreateMessage(string id, long createdAtUnixMs)
        => new(
            id,
            "dialog:instance:peer",
            "incoming",
            "body",
            "delivered",
            null,
            createdAtUnixMs,
            createdAtUnixMs,
            null);
}
