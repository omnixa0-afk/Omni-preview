using Omni.Windows.App.Core;
using Omni.Windows.App.Presentation;

namespace Omni.Windows.App.Features.Chats;

internal static class ChatPresentationText
{
    // Empty-state heading (the big line above the prompt). Two variants: no dialog is
    // selected vs. a dialog is open but has no messages yet. Keeps the single empty-state
    // block from showing one fixed heading that only fits one of the two situations.
    // Three cases: chats to choose from; contacts but no conversation yet (Chats lists conversations
    // only, docs/decisions/omni-app-ux.md R1); nobody at all.
    public static string BuildTranscriptSelectionHeading(bool hasChats, bool hasContacts = false)
        => hasChats ? "Выберите чат" : hasContacts ? "Начните разговор" : "Здесь появятся ваши чаты";

    public static string BuildTranscriptEmptyHeading()
        => "Пока тихо";

    public static string BuildTranscriptChatSelectionPrompt(bool hasChats, bool hasContacts = false)
        => hasChats
            ? "Переписка откроется здесь."
            : hasContacts
                ? "Выберите человека в Контактах и нажмите «Написать» — разговор появится здесь."
                : "Добавьте первый контакт — по приглашению или Omni-ID.";

    public static string BuildTranscriptUnavailableText()
        => "История переписки недоступна.";

    public static string BuildTranscriptAwaitingFirstConnectionHistoryText()
        => "Контакт сохранён. Переписка начнётся, когда он появится в сети.";

    public static string BuildTranscriptEmptyText()
        => "Сообщений пока нет. Напишите первое сообщение, чтобы начать разговор.";

    public static string BuildTranscriptAwaitingFirstConnectionEmptyText()
        => "Контакт сохранён локально. Ждём первое подключение, после этого можно будет писать.";

    public static string BuildChatPreviewText(ChatMessageRecord? lastMessage)
    {
        if (lastMessage is null)
        {
            return "Сообщений пока нет.";
        }

        bool outgoing = lastMessage.Direction.Equals("outgoing", StringComparison.OrdinalIgnoreCase);
        string body = lastMessage.Body.Trim();
        if (string.IsNullOrWhiteSpace(body) && lastMessage.CallHistoryEvent is not null)
        {
            // The call line names its own direction ("Исходящий звонок"); "Вы:" on top repeats it.
            return MainWindowPresentation.BuildCallHistorySummary(lastMessage.CallHistoryEvent);
        }
        if (string.IsNullOrWhiteSpace(body) && lastMessage.Attachment is not null)
        {
            body = lastMessage.Attachment.Presentation switch
            {
                { IsPhoto: true } => "Фото",
                { IsVoice: true } => "Голосовое сообщение",
                { IsVideoNote: true } => "Видеосообщение",
                _ => $"Файл: {lastMessage.Attachment.FileName}",
            };
        }
        if (body.Length > 96)
        {
            body = body[..96] + "…";
        }

        // Omni is 1-on-1, so an incoming message's author is always the peer — whose name is
        // already the chat-list card title. Prefixing the preview with "{peer}: " just repeats
        // it; only tag your own messages ("Вы: …") to distinguish them.
        return outgoing ? $"Вы: {body}" : body;
    }
}
