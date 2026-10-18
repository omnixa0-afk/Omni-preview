namespace Omni.Windows.App.Core;

internal static partial class NativeMethods
{
    private static void LoadChatDeviceExports()
    {
        LoadExport(ref omniChatSendText, nameof(omni_chat_send_text));
        LoadExport(ref omniChatSendAttachment, nameof(omni_chat_send_attachment));
        LoadExport(ref omniChatRequestAttachmentDownload, nameof(omni_chat_request_attachment_download));
        LoadExport(ref omniChatRevokeAttachmentAccess, nameof(omni_chat_revoke_attachment_access));
        LoadExport(ref omniChatListMessages, nameof(omni_chat_list_messages));
        LoadExport(ref omniChatListMessagesPage, nameof(omni_chat_list_messages_page));
        LoadExport(ref omniChatGetHeaderSummary, nameof(omni_chat_get_header_summary));
        LoadExport(ref omniChatGetScreenSnapshot, nameof(omni_chat_get_screen_snapshot));
        LoadExport(ref omniChatOpenSessionLease, nameof(omni_chat_open_session_lease));
        LoadExport(ref omniChatMarkRead, nameof(omni_chat_mark_read));
        LoadExport(ref omniChatEndSession, nameof(omni_chat_end_session));
        LoadExport(ref omniChatListThreadProjections, nameof(omni_chat_list_thread_projections));
        LoadExport(ref omniChatGetThreadListRefreshSummary, nameof(omni_chat_get_thread_list_refresh_summary));
        LoadExport(ref omniChatGetSecuritySummary, nameof(omni_chat_get_security_summary));
        LoadExport(ref omniChatGetMessageRefreshSummary, nameof(omni_chat_get_message_refresh_summary));
        LoadExport(ref omniChatGetMessageDelta, nameof(omni_chat_get_message_delta));
        LoadExport(ref omniChatGetMessageSecurityDetails, nameof(omni_chat_get_message_security_details));
        LoadExport(ref omniChatGetMessageSecurityDetailsBatch, nameof(omni_chat_get_message_security_details_batch));
    }
}
