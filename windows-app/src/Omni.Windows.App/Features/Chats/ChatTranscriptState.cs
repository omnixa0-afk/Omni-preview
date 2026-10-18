using Omni.Windows.App.Core;

namespace Omni.Windows.App.Features.Chats;

internal sealed class ActiveChatTranscriptState
{
    public string? DialogKey { get; set; }

    public long LoadVersion { get; set; }

    public DialogDeltaCursor? LiveCursor { get; set; }

    public bool ServerHasOlderMessages { get; set; }

    public bool HasTrimmedOlderMessages { get; set; }

    public bool IsLoadingOlderMessages { get; set; }

    public bool IsRestoringAnchor { get; set; }

    public long? OldestLoadedCreatedAtUnixMs { get; set; }

    public string? OldestLoadedMessageId { get; set; }

    public bool HasOlderMessages => ServerHasOlderMessages || HasTrimmedOlderMessages;
}

internal sealed record TranscriptScrollAnchor(
    string? MessageId,
    double OffsetFromViewportTop,
    double VerticalOffset);
