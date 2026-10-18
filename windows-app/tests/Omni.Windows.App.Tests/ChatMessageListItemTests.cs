using Microsoft.UI.Xaml;
using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Chats;
using Omni.Windows.App.Presentation;
using Xunit;

namespace Omni.Windows.App.Tests;

public sealed class ChatMessageListItemTests
{
    [Fact]
    public void ApplyFrom_UpdatesExistingInstance_ForStatusAndAttachmentChanges()
    {
        ChatMessageListItem item = new("msg-1");
        List<string> changedProperties = [];
        item.PropertyChanged += (_, args) =>
        {
            if (!string.IsNullOrWhiteSpace(args.PropertyName))
            {
                changedProperties.Add(args.PropertyName!);
            }
        };

        AttachmentRecord requestedAttachment = CreateAttachment(
            messageId: "msg-1",
            state: "requested",
            bytesTransferred: 20,
            fileSizeBytes: 100,
            localBlobAvailable: false,
            localServingEnabled: false,
            remoteHolderAvailable: true,
            localSavedPath: null);
        ChatMessageRecord sendingMessage = CreateMessage(
            id: "msg-1",
            status: "sending",
            body: "hello",
            attachment: requestedAttachment);
        MessageSecurityDetails initialSecurity = CreateSecurityDetails("msg-1", "encrypted_pending");

        item.ApplyFrom(sendingMessage, initialSecurity, null, null);
        item.UpdateBubbleMaxWidth(480d);

        changedProperties.Clear();

        AttachmentRecord downloadedAttachment = CreateAttachment(
            messageId: "msg-1",
            state: "downloaded",
            bytesTransferred: 100,
            fileSizeBytes: 100,
            localBlobAvailable: true,
            localServingEnabled: false,
            remoteHolderAvailable: false,
            localSavedPath: @"C:\downloads\hello.bin");
        ChatMessageRecord deliveredMessage = CreateMessage(
            id: "msg-1",
            status: "delivered",
            body: string.Empty,
            attachment: downloadedAttachment);
        MessageSecurityDetails updatedSecurity = CreateSecurityDetails("msg-1", "encrypted_delivered");

        ChatMessageListItem sameInstance = item;
        item.ApplyFrom(deliveredMessage, updatedSecurity, null, null);

        Assert.Same(sameInstance, item);
        Assert.Same(deliveredMessage, item.SourceMessage);
        Assert.Same(updatedSecurity, item.SecurityDetails);
        Assert.Equal(MainWindowPresentation.MapMessageStatus("delivered"), item.StatusText);
        Assert.Equal(MessageDeliveryVisualState.Delivered, item.DeliveryVisualState);
        Assert.Equal(Visibility.Visible, item.DeliveryIndicatorVisibility);
        Assert.Equal(Visibility.Visible, item.DeliverySecondaryGlyphVisibility);
        Assert.False(item.DeliveryIndicatorIsError);
        Assert.Equal("Доставлено", item.DeliveryIndicatorToolTipText);
        Assert.Equal(string.Empty, item.Body);
        Assert.Equal(Visibility.Collapsed, item.BodyVisibility);
        Assert.Equal(
            MainWindowPresentation.BuildAttachmentStatusText(downloadedAttachment, isOutgoing: true),
            item.AttachmentStatusText);
        Assert.Equal(480d, item.BubbleMaxWidth);
        Assert.Equal(Visibility.Collapsed, item.AttachmentProgressVisibility);
        Assert.Equal(100d, item.AttachmentProgressValue);
        Assert.True(item.AttachmentHasLocalCopy);
        Assert.Contains(nameof(ChatMessageListItem.StatusText), changedProperties);
        Assert.Contains(nameof(ChatMessageListItem.BodyVisibility), changedProperties);
        Assert.Contains(nameof(ChatMessageListItem.AttachmentProgressVisibility), changedProperties);
        Assert.DoesNotContain(nameof(ChatMessageListItem.BubbleMaxWidth), changedProperties);
    }

    [Fact]
    public void CreateChatItem_DerivesOutgoingBubblePresentation()
    {
        AttachmentRecord attachment = CreateAttachment(
            messageId: "msg-2",
            state: "available",
            bytesTransferred: 0,
            fileSizeBytes: 2048,
            localBlobAvailable: true,
            localServingEnabled: true,
            remoteHolderAvailable: true,
            localSavedPath: null);
        ChatMessageRecord message = CreateMessage(
            id: "msg-2",
            status: "queued",
            body: "payload",
            attachment: attachment);

        ChatMessageListItem item = MainWindowPresentation.CreateChatItem(message, null, null, null);

        Assert.Equal("msg-2", item.MessageId);
        Assert.True(item.IsOutgoing);
        Assert.Equal(HorizontalAlignment.Right, item.BubbleHorizontalAlignment);
        Assert.Equal(new Thickness(72, 0, 0, 10), item.BubbleMargin); // a lone message ends its run
        Assert.Equal(620d, item.BubbleMaxWidth);
        Assert.Equal(Visibility.Visible, item.BodyVisibility);
        Assert.Equal(Visibility.Visible, item.AttachmentVisibility);
        Assert.Equal("hello.bin", item.AttachmentFileName);
        Assert.Equal(MainWindowPresentation.FormatFileSize(2048), item.AttachmentSizeText);
        Assert.Equal(MainWindowPresentation.MapMessageStatus("queued"), item.StatusText);
        Assert.Equal(MessageDeliveryVisualState.Pending, item.DeliveryVisualState);
        Assert.Equal(Visibility.Visible, item.DeliveryIndicatorVisibility);
        Assert.Equal(Visibility.Collapsed, item.DeliverySecondaryGlyphVisibility);
        Assert.False(item.DeliveryIndicatorIsError);
        Assert.Equal("Статус сообщения: В очереди", item.DeliveryIndicatorAutomationName);
    }

    [Fact]
    public void FormatFileSize_UsesTerabytes_ForHugeAttachments()
    {
        Assert.Equal($"{100d:0.0} GB", MainWindowPresentation.FormatFileSize(100L * 1024 * 1024 * 1024));
        Assert.Equal($"{1d:0.0} TB", MainWindowPresentation.FormatFileSize(1024L * 1024 * 1024 * 1024));
    }

    [Fact]
    public void UpdateBubbleMaxWidth_RaisesPropertyChanged_OnlyWhenValueChanges()
    {
        ChatMessageListItem item = new("msg-4");
        List<string> changedProperties = [];
        item.PropertyChanged += (_, args) =>
        {
            if (!string.IsNullOrWhiteSpace(args.PropertyName))
            {
                changedProperties.Add(args.PropertyName!);
            }
        };

        item.UpdateBubbleMaxWidth(480d);
        item.UpdateBubbleMaxWidth(480d);

        Assert.Equal(480d, item.BubbleMaxWidth);
        Assert.Single(changedProperties.FindAll(name => name == nameof(ChatMessageListItem.BubbleMaxWidth)));
    }

    [Fact]
    public void CreateChatItem_HidesDeliveryIndicator_ForIncomingMessages()
    {
        ChatMessageRecord message = CreateMessage(
            id: "msg-3",
            status: "delivered",
            body: "incoming",
            attachment: null,
            direction: "incoming");

        ChatMessageListItem item = MainWindowPresentation.CreateChatItem(message, null, null, null);

        Assert.False(item.IsOutgoing);
        Assert.Equal(MessageDeliveryVisualState.None, item.DeliveryVisualState);
        Assert.Equal(Visibility.Collapsed, item.DeliveryIndicatorVisibility);
        Assert.Equal(string.Empty, item.DeliveryIndicatorToolTipText);
        Assert.Equal(string.Empty, item.DeliveryIndicatorAutomationName);
    }

    private static ChatMessageRecord CreateMessage(
        string id,
        string status,
        string body,
        AttachmentRecord? attachment,
        string direction = "outgoing")
        => new(
            Id: id,
            PeerKey: "dialog:instance:peer-1",
            Direction: direction,
            Body: body,
            Status: status,
            RemoteDeliveredAtUnixMs: null,
            CreatedAtUnixMs: 1_700_000_000_000,
            UpdatedAtUnixMs: 1_700_000_000_500,
            Attachment: attachment);

    private static AttachmentRecord CreateAttachment(
        string messageId,
        string state,
        long bytesTransferred,
        long fileSizeBytes,
        bool localBlobAvailable,
        bool localServingEnabled,
        bool remoteHolderAvailable,
        string? localSavedPath)
        => new(
            MessageId: messageId,
            FileName: "hello.bin",
            FileSizeBytes: fileSizeBytes,
            MimeType: "application/octet-stream",
            ContentIdHex: "abc123",
            ContentEncryption: "e2ee_media",
            State: state,
            BytesTransferred: bytesTransferred,
            LocalBlobAvailable: localBlobAvailable,
            LocalServingEnabled: localServingEnabled,
            RemoteHolderAvailable: remoteHolderAvailable,
            LocalSavedPath: localSavedPath,
            LastError: null);

    private static MessageSecurityDetails CreateSecurityDetails(string messageId, string encryptionState)
        => new(
            MessageId: messageId,
            DialogKey: "dialog:instance:peer-1",
            ContactId: "contact-1",
            RemoteAccountId: "acct-1",
            RemoteDeviceId: "device-1",
            RemoteDeviceLabel: "Peer device",
            SessionAddress: "session-1",
            TrustState: "verified",
            EncryptionState: encryptionState,
            BootstrapState: null,
            PayloadKind: "text",
            HasEncryptedPayload: true);
}
