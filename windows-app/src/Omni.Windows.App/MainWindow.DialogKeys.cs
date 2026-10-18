using Omni.Windows.App.Core;

namespace Omni.Windows.App;

public sealed partial class MainWindow
{
    private const string AccountDialogPrefix = "dialog:account:";

    private static bool IsAccountDialogKey(string? dialogKey)
        => !string.IsNullOrWhiteSpace(dialogKey) &&
           dialogKey.StartsWith(AccountDialogPrefix, StringComparison.Ordinal);

    private static string? BuildDialogKeyFromAccountId(string? accountId)
    {
        string trimmed = accountId?.Trim() ?? string.Empty;
        return string.IsNullOrWhiteSpace(trimmed)
            ? null
            : $"{AccountDialogPrefix}{trimmed}";
    }

    private bool CanComposeInCurrentChat()
    {
        ContactSnapshot? contact = currentChatHeaderSummary?.ContactCard?.Contact;
        if (contact is null ||
            !string.Equals(contact.RelationshipState, "approved", StringComparison.OrdinalIgnoreCase))
        {
            return false;
        }

        if (!IsAccountDialogKey(currentChatHeaderSummary?.DialogKey))
        {
            return true;
        }

        return !string.IsNullOrWhiteSpace(contact.RemoteInstanceId);
    }
}
