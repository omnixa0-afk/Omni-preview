using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Chats;
using Omni.Windows.App.Presentation;
using Omni.Windows.App.ViewModels;
using Windows.Foundation;

namespace Omni.Windows.App;

// Transcript viewport: scroll-position tracking, scroll-anchor capture/restore, near-bottom
// detection, bubble max-width sizing, older-message lazy paging, transcript-window trimming,
// and transcript load cancellation. Partial of MainWindow split from MainWindow.Chats.cs;
// same class, identical IL, no behaviour change.
public sealed partial class MainWindow
{
    private bool IsMessagesListNearBottom(double threshold = MessagesNearBottomThresholdPx)
    {
        EnsureMessagesScrollViewerHooked();
        ScrollViewer? scrollViewer = messagesListScrollViewer ?? FindDescendant<ScrollViewer>(MessagesListView);
        if (scrollViewer is null)
        {
            return true;
        }

        double remaining = scrollViewer.ScrollableHeight - scrollViewer.VerticalOffset;
        return remaining <= threshold;
    }

    private static T? FindDescendant<T>(DependencyObject root)
        where T : DependencyObject
    {
        if (root is T typed)
        {
            return typed;
        }

        int childCount = VisualTreeHelper.GetChildrenCount(root);
        for (int index = 0; index < childCount; index++)
        {
            DependencyObject child = VisualTreeHelper.GetChild(root, index);
            T? match = FindDescendant<T>(child);
            if (match is not null)
            {
                return match;
            }
        }

        return null;
    }

    private void OnMessagesListViewLoaded(object sender, RoutedEventArgs e)
    {
        EnsureMessagesScrollViewerHooked();
        RefreshTranscriptBubbleMaxWidth();
    }

    private void OnMessagesHostBorderSizeChanged(object sender, SizeChangedEventArgs e)
    {
        RefreshTranscriptBubbleMaxWidth(e.NewSize.Width);
        // A resize keeps the offset from the TOP, so a chat that sat on its latest message slid up
        // when the window grew (seen 2026-10-10: opened, then maximised — the middle of the chat).
        // Read at the bottom before the resize, stay at the bottom after it.
        if (transcriptSatAtBottom && e.NewSize.Height > 0 && e.NewSize.Height != e.PreviousSize.Height)
        {
            ScrollTranscriptToBottom();
        }
    }

    // Whether the reader was on the latest message at the last scroll — kept by ViewChanged, since
    // during a resize the offset has already moved.
    private bool transcriptSatAtBottom = true;

    private void ApplyCurrentTranscriptBubbleMaxWidth(ChatMessageListItem item)
        => item.UpdateBubbleMaxWidth(currentTranscriptBubbleMaxWidth);

    private void ApplyCurrentTranscriptBubbleMaxWidthToVisibleItems()
    {
        foreach (ChatMessageListItem item in messageItems)
        {
            ApplyCurrentTranscriptBubbleMaxWidth(item);
        }
    }

    // What a transcript row spends besides the bubble: the list's 8 off the edge, the row's 16 + 20
    // padding (ChatsSectionControl.xaml) and the 34 + 8 avatar column.
    private const double TranscriptRowChromeWidth = 8 + 16 + 20 + 34 + 8;

    private void UpdateCurrentTranscriptBubbleMaxWidth(double hostWidth)
    {
        currentTranscriptBubbleMaxWidth = MainWindowPresentation.CalculateTranscriptBubbleMaxWidth(
            hostWidth,
            TranscriptRowChromeWidth);
    }

    private void RefreshTranscriptBubbleMaxWidth()
        => RefreshTranscriptBubbleMaxWidth(MessagesHostBorder.ActualWidth);

    private void RefreshTranscriptBubbleMaxWidth(double hostWidth)
    {
        UpdateCurrentTranscriptBubbleMaxWidth(hostWidth);
        ApplyCurrentTranscriptBubbleMaxWidthToVisibleItems();
    }

    private void EnsureMessagesScrollViewerHooked()
    {
        if (messagesListScrollViewerHooked && messagesListScrollViewer is not null)
        {
            return;
        }

        ScrollViewer? scrollViewer = FindDescendant<ScrollViewer>(MessagesListView);
        if (scrollViewer is null)
        {
            return;
        }

        if (messagesListScrollViewer is not null)
        {
            messagesListScrollViewer.ViewChanged -= OnMessagesListViewViewChanged;
        }

        messagesListScrollViewer = scrollViewer;
        messagesListScrollViewer.ViewChanged += OnMessagesListViewViewChanged;
        messagesListScrollViewerHooked = true;
    }

    private void OnMessagesListViewViewChanged(object? sender, ScrollViewerViewChangedEventArgs e)
    {
        if (messagesListScrollViewer is not null && !e.IsIntermediate)
        {
            transcriptSatAtBottom = messagesListScrollViewer.ScrollableHeight - messagesListScrollViewer.VerticalOffset
                <= MessagesNearBottomThresholdPx;
        }

        if (messagesListScrollViewer is null ||
            activeChatTranscriptState.IsLoadingOlderMessages ||
            activeChatTranscriptState.IsRestoringAnchor ||
            !activeChatTranscriptState.HasOlderMessages)
        {
            return;
        }

        if (messagesListScrollViewer.VerticalOffset <= TranscriptTopLoadThresholdPx)
        {
            LoadOlderMessagesPage();
        }
    }

    private void LoadOlderMessagesPage()
    {
        string? dialogKey = GetResolvedCurrentDialogKey();
        if (string.IsNullOrWhiteSpace(dialogKey) ||
            activeChatTranscriptState.IsLoadingOlderMessages ||
            activeChatTranscriptState.IsRestoringAnchor ||
            !activeChatTranscriptState.HasOlderMessages ||
            !activeChatTranscriptState.OldestLoadedCreatedAtUnixMs.HasValue ||
            string.IsNullOrWhiteSpace(activeChatTranscriptState.OldestLoadedMessageId))
        {
            return;
        }

        EnsureMessagesScrollViewerHooked();
        TranscriptScrollAnchor? anchor = CaptureTranscriptScrollAnchor();
        activeChatTranscriptState.IsLoadingOlderMessages = true;

        CancellationTokenSource cancellation = ReplaceTranscriptOlderLoadCancellation();
        _ = LoadOlderMessagesPageAsync(
            dialogKey,
            activeChatTranscriptState.LoadVersion,
            activeChatTranscriptState.OldestLoadedCreatedAtUnixMs.Value,
            activeChatTranscriptState.OldestLoadedMessageId!,
            anchor,
            cancellation);
    }

    private async Task LoadOlderMessagesPageAsync(
        string dialogKey,
        long loadVersion,
        long beforeCreatedAtUnixMs,
        string beforeMessageId,
        TranscriptScrollAnchor? anchor,
        CancellationTokenSource cancellation)
    {
        bool applyQueued = false;

        try
        {
            TranscriptHistoryPagePayload? payload = await Task.Run(() => chatRefreshService.FetchOlderMessagesPage(
                rustBridge,
                dialogKey,
                loadVersion,
                beforeCreatedAtUnixMs,
                beforeMessageId,
                cancellation.Token),
                cancellation.Token);
            if (payload is null)
            {
                return;
            }

            applyQueued = DispatcherQueue.TryEnqueue(() => ApplyOlderMessagesPage(payload, anchor));
        }
        catch (OperationCanceledException)
        {
            // A newer request superseded this page load.
        }
        finally
        {
            if (!applyQueued && IsTranscriptResultCurrent(dialogKey, loadVersion))
            {
                activeChatTranscriptState.IsLoadingOlderMessages = false;
            }

            if (ReferenceEquals(transcriptOlderLoadCancellation, cancellation))
            {
                transcriptOlderLoadCancellation = null;
            }

            cancellation.Dispose();
        }
    }

    private TranscriptScrollAnchor? CaptureTranscriptScrollAnchor()
    {
        EnsureMessagesScrollViewerHooked();
        if (messagesListScrollViewer is null || messageItems.Count == 0)
        {
            return null;
        }

        foreach (ChatMessageListItem item in messageItems)
        {
            if (MessagesListView.ContainerFromItem(item) is not ListViewItem container)
            {
                continue;
            }

            double top = GetElementTopRelativeToViewport(container, messagesListScrollViewer);
            double bottom = top + container.ActualHeight;
            if (bottom >= 0)
            {
                return new TranscriptScrollAnchor(
                    item.MessageId,
                    top,
                    messagesListScrollViewer.VerticalOffset);
            }
        }

        return new TranscriptScrollAnchor(
            messageItems[0].MessageId,
            0d,
            messagesListScrollViewer.VerticalOffset);
    }

    private void ScrollTranscriptToBottom(bool disableAnimation = true)
    {
        EnsureMessagesScrollViewerHooked();
        if (messagesListScrollViewer is null)
        {
            return;
        }

        // The list is virtualized: ScrollableHeight is an estimate from the bubbles built so far, and
        // the ones below grow it once built (a file bubble is three text bubbles tall). One jump to
        // "the end" landed a few messages short of the latest one (seen live 2026-10-10: a chat with
        // file bubbles opened above its last two messages). Bring the last item in, then jump again
        // until the extent stops growing.
        if (messageItems.Count > 0)
        {
            MessagesListView.ScrollIntoView(messageItems[^1], ScrollIntoViewAlignment.Leading);
        }

        MessagesListView.UpdateLayout();
        messagesListScrollViewer.ChangeView(
            null,
            messagesListScrollViewer.ScrollableHeight,
            null,
            disableAnimation);
        SettleTranscriptAtBottom(disableAnimation, TranscriptBottomSettlePasses);
    }

    private const int TranscriptBottomSettlePasses = 4;

    private void SettleTranscriptAtBottom(bool disableAnimation, int passesLeft)
    {
        DispatcherQueue.TryEnqueue(DispatcherQueuePriority.Low, () =>
        {
            EnsureMessagesScrollViewerHooked();
            if (messagesListScrollViewer is null)
            {
                return;
            }

            double extentBefore = messagesListScrollViewer.ScrollableHeight;
            MessagesListView.UpdateLayout();
            double bottom = messagesListScrollViewer.ScrollableHeight;
            messagesListScrollViewer.ChangeView(null, bottom, null, disableAnimation);
            if (passesLeft > 1 && (bottom > extentBefore + 0.5 || messagesListScrollViewer.VerticalOffset < bottom - 0.5))
            {
                SettleTranscriptAtBottom(disableAnimation, passesLeft - 1);
            }
        });
    }

    private void RestoreTranscriptScrollAnchor(TranscriptScrollAnchor anchor)
    {
        EnsureMessagesScrollViewerHooked();
        if (messagesListScrollViewer is null)
        {
            activeChatTranscriptState.IsRestoringAnchor = false;
            return;
        }

        activeChatTranscriptState.IsRestoringAnchor = true;
        if (string.IsNullOrWhiteSpace(anchor.MessageId) ||
            !currentChatMessageItemsById.TryGetValue(anchor.MessageId, out ChatMessageListItem? anchorItem))
        {
            messagesListScrollViewer.ChangeView(null, anchor.VerticalOffset, null, true);
            activeChatTranscriptState.IsRestoringAnchor = false;
            return;
        }

        MessagesListView.ScrollIntoView(anchorItem);
        bool restoreQueued = DispatcherQueue.TryEnqueue(() =>
        {
            try
            {
                EnsureMessagesScrollViewerHooked();
                if (messagesListScrollViewer is null ||
                    !currentChatMessageItemsById.TryGetValue(anchor.MessageId, out ChatMessageListItem? currentAnchorItem))
                {
                    return;
                }

                MessagesListView.UpdateLayout();
                if (MessagesListView.ContainerFromItem(currentAnchorItem) is not ListViewItem container)
                {
                    messagesListScrollViewer.ChangeView(null, anchor.VerticalOffset, null, true);
                    return;
                }

                double currentTop = GetElementTopRelativeToViewport(container, messagesListScrollViewer);
                double targetOffset = messagesListScrollViewer.VerticalOffset + (currentTop - anchor.OffsetFromViewportTop);
                messagesListScrollViewer.ChangeView(null, Math.Max(0d, targetOffset), null, true);
            }
            finally
            {
                activeChatTranscriptState.IsRestoringAnchor = false;
            }
        });

        if (!restoreQueued)
        {
            activeChatTranscriptState.IsRestoringAnchor = false;
        }
    }

    private bool IsTranscriptResultCurrent(string dialogKey, long loadVersion)
    {
        if (loadVersion != activeChatTranscriptState.LoadVersion)
        {
            return false;
        }

        string? currentDialogKey = GetResolvedCurrentDialogKey();
        return string.Equals(activeChatTranscriptState.DialogKey, dialogKey, StringComparison.Ordinal) ||
               string.Equals(currentDialogKey, dialogKey, StringComparison.Ordinal);
    }

    private bool IsMessageOlderThanCurrentTranscriptWindow(ChatMessageRecord message)
        => TranscriptWindowing.IsMessageOlderThanWindow(
            message,
            activeChatTranscriptState.OldestLoadedCreatedAtUnixMs,
            activeChatTranscriptState.OldestLoadedMessageId);

    private void RecalculateTranscriptOldestBoundary(
        long? fallbackCreatedAtUnixMs = null,
        string? fallbackMessageId = null)
    {
        TranscriptWindowBoundary boundary = TranscriptWindowing.FindOldestBoundary(
            messageItems,
            fallbackCreatedAtUnixMs,
            fallbackMessageId);
        activeChatTranscriptState.OldestLoadedCreatedAtUnixMs = boundary.CreatedAtUnixMs;
        activeChatTranscriptState.OldestLoadedMessageId = boundary.MessageId;
    }

    private void TryTrimTranscriptWindow()
    {
        if (messageItems.Count <= ChatRefreshService.TranscriptHardItemLimit ||
            activeChatTranscriptState.IsLoadingOlderMessages ||
            activeChatTranscriptState.IsRestoringAnchor)
        {
            return;
        }

        int removeCount = TranscriptWindowing.CalculateTrimCount(
            messageItems.Count,
            IsMessagesListNearBottom(MessagesNearBottomThresholdPx),
            activeChatTranscriptState.IsLoadingOlderMessages,
            activeChatTranscriptState.IsRestoringAnchor,
            ChatRefreshService.TranscriptHardItemLimit,
            ChatRefreshService.TranscriptSoftItemLimit);
        if (removeCount <= 0)
        {
            return;
        }

        for (int index = 0; index < removeCount; index++)
        {
            string messageId = messageItems[0].MessageId;
            messageItems.RemoveAt(0);
            currentChatMessageItemsById.Remove(messageId);
            currentChatMessagesById.Remove(messageId);
            currentChatSecurityDetailsByMessageId.Remove(messageId);
        }

        activeChatTranscriptState.HasTrimmedOlderMessages = true;
        RecalculateTranscriptOldestBoundary();
    }

    private CancellationTokenSource ReplaceTranscriptTailLoadCancellation()
    {
        CancelTranscriptTailLoad();
        transcriptTailLoadCancellation = new CancellationTokenSource();
        return transcriptTailLoadCancellation;
    }

    private CancellationTokenSource ReplaceTranscriptOlderLoadCancellation()
    {
        CancelTranscriptOlderLoad();
        transcriptOlderLoadCancellation = new CancellationTokenSource();
        return transcriptOlderLoadCancellation;
    }

    private void CancelTranscriptLoads()
    {
        CancelTranscriptTailLoad();
        CancelTranscriptOlderLoad();
        activeChatTranscriptState.IsLoadingOlderMessages = false;
        activeChatTranscriptState.IsRestoringAnchor = false;
    }

    private void CancelTranscriptTailLoad()
    {
        CancellationTokenSource? cancellation = transcriptTailLoadCancellation;
        transcriptTailLoadCancellation = null;
        if (cancellation is null)
        {
            return;
        }

        cancellation.Cancel();
        cancellation.Dispose();
    }

    private void CancelTranscriptOlderLoad()
    {
        CancellationTokenSource? cancellation = transcriptOlderLoadCancellation;
        transcriptOlderLoadCancellation = null;
        if (cancellation is null)
        {
            return;
        }

        cancellation.Cancel();
        cancellation.Dispose();
    }

    private static double GetElementTopRelativeToViewport(FrameworkElement element, UIElement viewport)
        => element.TransformToVisual(viewport).TransformPoint(new Point(0, 0)).Y;
}
