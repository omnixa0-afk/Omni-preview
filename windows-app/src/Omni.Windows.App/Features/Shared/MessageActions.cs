using System.IO;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Omni.Windows.App.Core;
using Omni.Windows.App.Presentation;
using Windows.ApplicationModel.DataTransfer;

namespace Omni.Windows.App.Features.Shared;

/// What every conversation does the same with its messages: asks before deleting, picks where to
/// forward and sends there, copies several.
internal static class MessageActions
{
    /// Test channel: the answer a dialog would have got, for `ui.answer`.
    internal static bool? UiTestDeleteForEveryone { get; set; }

    internal static string? UiTestForwardTo { get; set; }

    /// Who is on the mesh right now, for the people's lines in the forward list.
    internal static Func<IReadOnlySet<string>>? LinkedAccounts { get; set; }

    /// The person's answer: null to keep them, else whether to delete for everyone. `everyone` names
    /// who else loses them; null when only here is possible.
    internal static async Task<bool?> AskDeleteAsync(XamlRoot root, int count, string? everyone, bool everyoneOnly = false)
    {
        if (UiTestDeleteForEveryone is { } answered)
        {
            UiTestDeleteForEveryone = null;
            return answered;
        }

        CheckBox forEveryone = new() { Content = everyone ?? string.Empty, IsChecked = true };
        string question = count == 1 ? "Удалить это сообщение?" : $"Удалить сообщения ({count})?";
        StackPanel content = new() { Spacing = 8 };
        content.Children.Add(new TextBlock { Text = question, TextWrapping = TextWrapping.Wrap });
        if (everyone is not null && !everyoneOnly)
        {
            content.Children.Add(forEveryone);
        }

        ContentDialog dialog = new()
        {
            XamlRoot = root,
            RequestedTheme = MainWindowPresentation.ActiveTheme,
            Title = "Удаление",
            Content = content,
            PrimaryButtonText = "Удалить",
            CloseButtonText = "Отмена",
            DefaultButton = ContentDialogButton.Close,
        };
        if (await dialog.ShowAsync() != ContentDialogResult.Primary)
        {
            return null;
        }

        return everyoneOnly || (everyone is not null && forEveryone.IsChecked == true);
    }

    /// Texts of the rows, one under another, into the clipboard.
    internal static void Copy(IEnumerable<MessageRow> rows)
    {
        string text = string.Join("\n\n", rows.Select(r => r.Body).Where(b => b.Length > 0));
        if (text.Length == 0)
        {
            return;
        }

        DataPackage package = new();
        package.SetText(text);
        Clipboard.SetContent(package);
    }

    /// Picks chats and groups and sends the rows there. What it could not send, it says; null when all went.
    internal static async Task<string?> ForwardAsync(XamlRoot root, RustBridge bridge, IReadOnlyList<MessageRow> rows)
    {
        IReadOnlySet<string> linked = LinkedAccounts?.Invoke() ?? new HashSet<string>();
        List<ForwardChoice> targets = await Task.Run(() => ListTargets(bridge, linked));
        List<ForwardChoice>? chosen = UiTestForwardTo is { } wanted
            ? targets.Where(t => t.Title.Contains(wanted, StringComparison.OrdinalIgnoreCase)).ToList()
            : await PickAsync(root, targets, rows.Count);
        UiTestForwardTo = null;
        if (chosen is null || chosen.Count == 0)
        {
            return null;
        }

        return await Task.Run(() =>
        {
            int failed = 0;
            int notHere = 0;
            foreach (ForwardChoice target in chosen)
            {
                foreach (MessageRow row in rows)
                {
                    bool hasFile = row.AttachmentVisibility == Visibility.Visible;
                    if (hasFile && (row.AttachmentLocalPath is not { Length: > 0 } path || !File.Exists(path)))
                    {
                        notHere++;
                        continue;
                    }

                    try
                    {
                        Send(bridge, target, row, hasFile ? row.AttachmentLocalPath : null);
                    }
                    catch (Exception)
                    {
                        failed++;
                    }
                }
            }

            if (notHere > 0)
            {
                return "Часть файлов ещё не скачана: их не переслать. Скачайте и повторите.";
            }

            return failed > 0 ? "Не всё удалось переслать: проверьте, кому ушло." : null;
        });
    }

    private static void Send(RustBridge bridge, ForwardChoice target, MessageRow row, string? file)
    {
        bool asPhoto = row.PhotoVisibility == Visibility.Visible;
        bool asVoice = row.VoiceVisibility == Visibility.Visible;
        bool asVideoNote = row.VideoNoteVisibility == Visibility.Visible;
        if (target.GroupId is { } groupId)
        {
            if (file is not null)
            {
                bridge.SendGroupAttachment(groupId, row.Body, file, row.AttachmentFileName, asPhoto, asVoice: asVoice, asVideoNote: asVideoNote);
            }
            else
            {
                bridge.SendGroupText(groupId, row.Body);
            }

            return;
        }

        if (file is not null)
        {
            bridge.SendAttachment(target.DialogKey, row.Body, file, row.AttachmentFileName, asPhoto, asVoice: asVoice, asVideoNote: asVideoNote);
        }
        else
        {
            bridge.SendText(target.DialogKey, row.Body);
        }
    }

    private static List<ForwardChoice> ListTargets(RustBridge bridge, IReadOnlySet<string> linked)
    {
        long now = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
        List<ForwardChoice> targets = [];
        foreach (RustBridge.ChatThreadProjection thread in bridge.ListChatThreadProjections())
        {
            ContactSnapshot contact = thread.Contact;
            if (contact.RelationshipState == "approved" && !contact.IsBlocked && contact.PrimaryDialogKey is { Length: > 0 } dialog)
            {
                targets.Add(new ForwardChoice
                {
                    Title = MainWindowPresentation.ContactTitle(contact),
                    Subtitle = MainWindowPresentation.MapContactPresenceText(
                        contact, MainWindowPresentation.MapContactPresenceDotKind(contact, now, linked)),
                    DialogKey = dialog,
                    AvatarPath = contact.PublicAvatarImagePath,
                    AvatarVersion = contact.PublicAvatarHashHex,
                    AvatarSeed = MainWindowPresentation.AvatarSeed(contact),
                    SortUnixMs = thread.LastMessage?.CreatedAtUnixMs ?? contact.UpdatedAtUnixMs,
                });
            }
        }

        foreach (GroupSummary group in bridge.ListGroups().Where(g => g.MyStatus == "member"))
        {
            targets.Add(new ForwardChoice
            {
                Title = group.Name,
                Subtitle = $"{group.MemberCount} {Groups.GroupConversationControl.MembersWord(group.MemberCount)}",
                DialogKey = group.DialogKey,
                GroupId = group.GroupId,
                AvatarPath = group.AvatarPath,
                AvatarVersion = group.AvatarPath,
                AvatarSeed = group.GroupId,
                SortUnixMs = group.LastActivityUnixMs,
            });
        }

        return targets.OrderByDescending(t => t.SortUnixMs).ToList();
    }

    private static async Task<List<ForwardChoice>?> PickAsync(XamlRoot root, List<ForwardChoice> targets, int count)
    {
        ForwardPicker picker = new(targets)
        {
            Width = ForwardPickerWidth,
            Height = Math.Clamp(root.Size.Height - ForwardDialogChrome, ForwardPickerLowest, ForwardPickerTallest),
        };
        ContentDialog dialog = new()
        {
            XamlRoot = root,
            RequestedTheme = MainWindowPresentation.ActiveTheme,
            Title = count == 1 ? "Переслать сообщение" : $"Переслать сообщения ({count})",
            Content = picker,
            CloseButtonText = "Отмена",
        };
        dialog.Resources["ContentDialogMaxHeight"] = ForwardPickerTallest + ForwardDialogChrome;
        List<ForwardChoice>? sentAtOnce = null;
        picker.SendTo += choice =>
        {
            sentAtOnce = [choice];
            dialog.Hide();
        };
        picker.PickedChanged += () =>
        {
            int picked = picker.Picked.Count;
            dialog.PrimaryButtonText = picked > 0 ? $"Переслать ({picked})" : string.Empty;
        };
        dialog.Opened += (_, _) => picker.FocusSearch();
        ContentDialogResult result = await dialog.ShowAsync();
        return sentAtOnce ?? (result == ContentDialogResult.Primary ? picker.Picked : null);
    }

    private const double ForwardPickerWidth = 340;
    private const double ForwardPickerLowest = 240;
    private const double ForwardPickerTallest = 640;

    // The dialog's title, buttons and margins around the picker, and the window's edge.
    private const double ForwardDialogChrome = 200;
}
