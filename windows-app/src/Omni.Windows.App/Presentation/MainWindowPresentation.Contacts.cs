using Omni.Windows.App.Core;
using Omni.Windows.App.Features.Contacts;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Media.Imaging;

namespace Omni.Windows.App.Presentation;

/// <summary>Что показывает точка присутствия: живой линк, недавняя активность, ничего.</summary>
internal enum ContactPresenceDotKind
{
    None,
    Recent,
    Live,
}

internal static partial class MainWindowPresentation
{
    public static string MapTrustState(string trustState)
        => trustState.ToLowerInvariant() switch
        {
            "verified" => "Проверен",
            "reverifyneeded" or "reverify_needed" => "Нужно перепроверить",
            "identitychanged" or "identity_changed" => "Идентичность изменилась",
            _ => "Не проверен",
        };

    public static string FormatTimestamp(long unixMs)
        => DateTimeOffset.FromUnixTimeMilliseconds(unixMs).ToLocalTime().ToString("dd.MM.yyyy HH:mm:ss");

    private static readonly System.Globalization.CultureInfo Russian = System.Globalization.CultureInfo.GetCultureInfo("ru-RU");

    /// A date as a person says it: «сегодня в 23:17», «вчера в 9:05», «12 октября», «3 марта 2025».
    /// For what people read; FormatTimestamp (with seconds) stays for Diagnostics.
    public static string FormatHumanDate(long unixMs, DateTimeOffset? now = null)
    {
        DateTimeOffset at = DateTimeOffset.FromUnixTimeMilliseconds(unixMs).ToLocalTime();
        DateTime today = (now ?? DateTimeOffset.Now).ToLocalTime().Date;
        if (at.Date == today)
        {
            return $"сегодня в {at:H:mm}";
        }

        if (at.Date == today.AddDays(-1))
        {
            return $"вчера в {at:H:mm}";
        }

        return at.Year == today.Year ? at.ToString("d MMMM", Russian) : at.ToString("d MMMM yyyy", Russian);
    }

    // Compact chat-bubble timestamp: time-of-day for today, day + time for older messages.
    // (The full "dd.MM.yyyy HH:mm:ss" from FormatTimestamp reads as diagnostic noise inside a bubble.)
    public static string FormatMessageTime(long unixMs)
    {
        DateTimeOffset ts = DateTimeOffset.FromUnixTimeMilliseconds(unixMs).ToLocalTime();
        return ts.Date == DateTimeOffset.Now.ToLocalTime().Date
            ? ts.ToString("HH:mm")
            : ts.ToString("dd.MM HH:mm");
    }

    /// The time under a bubble: the day is said by the transcript's day headers.
    public static string FormatBubbleTime(long unixMs)
        => DateTimeOffset.FromUnixTimeMilliseconds(unixMs).ToLocalTime().ToString("HH:mm");

    private static readonly string[] GenitiveMonths =
    [
        "января", "февраля", "марта", "апреля", "мая", "июня",
        "июля", "августа", "сентября", "октября", "ноября", "декабря",
    ];

    /// The header over the first message of a day: «Сегодня», «Вчера», «8 октября», «8 октября 2025».
    public static string FormatDayHeader(long unixMs, DateTime today)
    {
        DateTime day = DateTimeOffset.FromUnixTimeMilliseconds(unixMs).ToLocalTime().Date;
        if (day == today.Date)
        {
            return "Сегодня";
        }

        if (day == today.Date.AddDays(-1))
        {
            return "Вчера";
        }

        string date = $"{day.Day} {GenitiveMonths[day.Month - 1]}";
        return day.Year == today.Year ? date : $"{date} {day.Year}";
    }

    /// What picks a contact's avatar colour (AvatarPalette): the account id — the same on every
    /// device and for every copy of the contact — else the contact id.
    public static string AvatarSeed(ContactSnapshot contact)
        => string.IsNullOrWhiteSpace(contact.RemoteAccountId) ? contact.ContactId : contact.RemoteAccountId!;

    public static string BuildAvatarText(string? title)
    {
        string source = string.IsNullOrWhiteSpace(title) ? "?" : title.Trim();
        string first = source.Length > 0 ? source[..1] : "?";
        return first.ToUpperInvariant();
    }

    // One image per (file, size, version): the chat list, the transcript and the group views rebuild
    // their rows every few seconds, and a NEW BitmapImage for the same file made the Image reload —
    // the avatar blinked (owner, live 2026-10-13: «аватарка раз в несколько секунд мерцает»). UI thread
    // only (a BitmapImage is). Bounded: a new version of a photo is a new key; the old ones go when
    // the cache grows past the cap.
    private static readonly Dictionary<string, BitmapImage> AvatarImages = new(StringComparer.Ordinal);
    private const int AvatarImagesCap = 512;

    // Which file each avatar picture was decoded from (AvatarView.PicturePath: a tap opens the viewer
    // on it). The picture itself comes from the core's plaintext, not from the path: profile media is
    // encrypted at rest (docs/decisions/db-at-rest-encryption.md, «Вложения»).
    private static readonly System.Runtime.CompilerServices.ConditionalWeakTable<BitmapImage, string> AvatarImageFiles = new();

    /// <summary>The file an avatar picture was decoded from, if it was one of ours.</summary>
    public static string? AvatarImageFile(ImageSource? image)
        => image is BitmapImage bitmap && AvatarImageFiles.TryGetValue(bitmap, out string? path) ? path : null;

    public static BitmapImage? BuildAvatarImage(
        string? imagePath,
        int decodePixelSize = 96,
        string? cacheBuster = null)
    {
        if (string.IsNullOrWhiteSpace(imagePath))
        {
            return null;
        }

        try
        {
            string fullPath = Path.GetFullPath(imagePath);
            if (!File.Exists(fullPath))
            {
                return null;
            }

            string key = $"{fullPath}|{decodePixelSize}|{cacheBuster}";
            if (AvatarImages.TryGetValue(key, out BitmapImage? known))
            {
                return known;
            }

            BitmapImage image = new();
            if (decodePixelSize > 0)
            {
                image.DecodePixelWidth = decodePixelSize;
            }

            byte[]? bytes = Core.MediaFiles.ReadAllBytes(fullPath);
            if (bytes is null)
            {
                return null;
            }

            // Not disposed here: the image may decode from it after SetSource returns.
            image.SetSource(new MemoryStream(bytes).AsRandomAccessStream());

            AvatarImageFiles.AddOrUpdate(image, fullPath);
            if (AvatarImages.Count >= AvatarImagesCap)
            {
                AvatarImages.Clear();
            }
            AvatarImages[key] = image;
            return image;
        }
        catch
        {
            return null;
        }
    }

    public static string BuildContactStatus(
        ContactSnapshot contact,
        long nowUnixMs,
        IReadOnlySet<string> linkedAccountIds)
    {
        string relationship = MapRelationshipState(contact.RelationshipState);
        if (contact.RelationshipState.Equals("pending", StringComparison.OrdinalIgnoreCase))
        {
            return BuildPendingContactStatus(contact);
        }

        if (!contact.RelationshipState.Equals("approved", StringComparison.OrdinalIgnoreCase))
        {
            return relationship;
        }

        if (!string.IsNullOrWhiteSpace(contact.RemoteAccountId) &&
            string.IsNullOrWhiteSpace(contact.RemoteInstanceId))
        {
            return "Сохранён локально";
        }

        // An approved contact's status is its presence alone: "В контактах" in front of it said
        // nothing on a screen that lists contacts (maintainers, 2026-10-10: «один статус»).
        if (!contact.HasMet)
        {
            return "Ещё не встречались";
        }

        return MapContactPresenceState(contact, nowUnixMs, linkedAccountIds);
    }

    /// <summary>
    /// The name a contact is shown under — everywhere (contact list, card, chat list, chat header,
    /// group pickers). The local name, unless it is only the placeholder the core fills in when a
    /// contact arrives without one (its account id: «acct-e929d2…» as a name, seen 2026-10-10);
    /// then the contact's public name, else «Без имени · e929…» — the id's start, so two unnamed
    /// contacts still differ.
    /// </summary>
    public static string ContactTitle(ContactSnapshot contact)
    {
        string? petname = contact.Petname?.Trim();
        // Also its first characters: contacts back from a passport mark were named so before 2026-10-13.
        bool placeholder = string.IsNullOrEmpty(petname)
            || string.Equals(petname, contact.RemoteAccountId, StringComparison.Ordinal)
            || (petname.StartsWith("acct-", StringComparison.Ordinal)
                && contact.RemoteAccountId?.StartsWith(petname, StringComparison.Ordinal) == true);
        if (!placeholder)
        {
            return petname!;
        }

        if (!string.IsNullOrWhiteSpace(contact.PublicDisplayName))
        {
            return contact.PublicDisplayName.Trim();
        }

        return contact.RemoteAccountId is { Length: > 0 } accountId
            ? $"Без имени · {ShortAccountId(accountId)}"
            : "Без имени";
    }

    /// «acct-e929d200…» → «e929…».
    public static string ShortAccountId(string accountId)
    {
        string hex = accountId.StartsWith("acct-", StringComparison.Ordinal) ? accountId[5..] : accountId;
        return hex.Length <= 4 ? hex : $"{hex[..4]}…";
    }

    public static ContactListItem ToContactItem(ContactSnapshot contact, long nowUnixMs, IReadOnlySet<string> linkedAccountIds)
    {
        string title = ContactTitle(contact);
        string status = BuildCompactContactStatus(contact, nowUnixMs, linkedAccountIds);
        string badge = BuildContactBadgeText(contact);
        string avatarText = BuildAvatarText(title);
        BitmapImage? avatarImage = BuildAvatarImage(
            contact.PublicAvatarImagePath,
            96,
            contact.PublicAvatarPendingHashHex ??
            contact.PublicAvatarHashHex ??
            contact.PublicProfileUpdatedAtUnixMs?.ToString());
        bool isAvatarLoading = !string.IsNullOrWhiteSpace(contact.PublicAvatarPendingHashHex);
        ContactPresenceDotKind presenceDotKind = MapContactPresenceDotKind(contact, nowUnixMs, linkedAccountIds);
        (Visibility presenceDotVisibility, Brush presenceDotBrush) = MapContactPresenceDot(presenceDotKind);

        return new ContactListItem(
            contact.ContactId,
            contact.RelationshipState,
            contact.PendingRequestDirection,
            contact.HasMet,
            avatarText,
            avatarImage,
            avatarImage is null ? Visibility.Collapsed : Visibility.Visible,
            avatarImage is null ? Visibility.Visible : Visibility.Collapsed,
            isAvatarLoading ? Visibility.Visible : Visibility.Collapsed,
            title,
            status,
            badge,
            presenceDotVisibility,
            presenceDotBrush,
            BuildContactItemSignature(contact, presenceDotKind, status),
            AvatarSeed(contact));
    }

    /// <summary>
    /// Ключ кэша строки списка контактов. Живость линка и готовый статус входят в него наравне с
    /// полями контакта: когда пир выключается, его объявленное присутствие просто ПЕРЕСТАЁТ
    /// обновляться — поля остаются прежними, и подпись без этих двух слагаемых не меняется. Список
    /// тогда остаётся с зелёной точкой на выключенной машине, пока шапка рядом честно пишет «Офлайн».
    /// Кэш обязан ключеваться по всему, от чего зависит показанное значение.
    /// </summary>
    internal static string BuildContactItemSignature(
        ContactSnapshot contact,
        ContactPresenceDotKind presenceDotKind,
        string status)
        // What the row SHOWS, not every input: the presence clocks move with each heartbeat, and a new
        // signature replaced the row every few seconds — the contact «дёрнулся» (owner, 2026-10-13).
        // Their effect is here already: the dot's decision and the status text.
        => $"{contact.ContactId}:{contact.Petname}:{contact.PublicDisplayName}:{contact.PublicAvatarHashHex}:{contact.PublicAvatarPendingHashHex}:{contact.PublicAvatarPendingMimeType}:{contact.PublicProfileUpdatedAtUnixMs}:{contact.RemoteAccountId}:{contact.RemoteInstanceId}:{contact.RelationshipState}:{contact.PendingRequestDirection}:{contact.HasMet}:{contact.RemovedAtUnixMs}:{presenceDotKind}:{status}";

    public static string BuildContactDetails(ContactCardSnapshot? card, long nowUnixMs)
    {
        if (card is null)
        {
            return "Выберите контакт слева или отправьте запрос на новый контакт.";
        }

        _ = nowUnixMs;
        ContactSnapshot contact = card.Contact;

        // Who it is, as a person reads it: the name they gave themselves (when ours differs) and
        // whether we removed them. The account id is the small line under it (ContactCardIdTextBlock),
        // for checking, not the headline (owner, 2026-10-13: UI/UX pass).
        List<string> lines = [];

        if (!string.IsNullOrWhiteSpace(contact.PublicDisplayName) &&
            !string.Equals(contact.PublicDisplayName, contact.Petname, StringComparison.Ordinal))
        {
            lines.Add($"Называет себя «{contact.PublicDisplayName}»");
        }

        if (contact.RemovedAtUnixMs.HasValue)
        {
            lines.Add($"Удалён {FormatHumanDate(contact.RemovedAtUnixMs.Value)}");
        }

        if (lines.Count == 0)
        {
            lines.Add("Аккаунт");
        }

        return string.Join('\n', lines);
    }

    /// The one line under «Безопасность»: verified or not, and since when.
    public static string BuildTrustLine(ContactCardSnapshot? card)
    {
        if (card is null)
        {
            return string.Empty;
        }

        return (card.TrustState ?? string.Empty).ToLowerInvariant() switch
        {
            "verified" => card.Verification.LastVerifiedAtUnixMs is long at
                ? $"Ключи сверены {FormatHumanDate(at)}"
                : "Ключи сверены вручную",
            "reverifyneeded" or "reverify_needed" => "Нужно сверить ключи заново",
            "identitychanged" or "identity_changed" => "Ключ собеседника сменился",
            _ => "Ключи не сверены",
        };
    }

    /// What the state means, in a sentence — none when all is well.
    public static string BuildTrustWarning(ContactCardSnapshot? card)
    {
        if (card is null)
        {
            return string.Empty;
        }

        return (card.TrustState ?? string.Empty).ToLowerInvariant() switch
        {
            "verified" => string.Empty,
            "reverifyneeded" or "reverify_needed" => "Прошлая сверка больше не действует — сверьте ещё раз.",
            "identitychanged" or "identity_changed" => "Это может быть новое устройство собеседника — или подмена. Сверьте ключи, прежде чем доверять.",
            _ => "Переписка и так зашифрована. Сверка ключей защищает от подмены собеседника — это по желанию.",
        };
    }

    public static string BuildAccountSummary(LocalAccountSnapshot? account)
    {
        if (account is null)
        {
            return "Локальный профиль ещё не создан.";
        }

        // One quiet line. The account id is the core's, not the person's: their address is the Omni-ID
        // («Приватность»), and the id itself is in the data for manual checking (Diagnostics →
        // «Инструменты») — owner, 2026-10-13: settings remade by a UI/UX pass.
        string created = DateTimeOffset.FromUnixTimeMilliseconds(account.CreatedAtUnixMs)
            .ToLocalTime()
            .ToString("d MMMM yyyy", System.Globalization.CultureInfo.GetCultureInfo("ru-RU"));
        return $"Профиль «{account.Descriptor.DisplayName}» создан {created}.";
    }

    public static string BuildRecoverySummary(
        LocalAccountSnapshot? account,
        PasswordProtectionSummary? passwordProtectionSummary,
        string? passwordProtectionErrorText)
    {
        if (account is null)
        {
            return "Фраза восстановления станет доступна после создания или восстановления профиля.";
        }

        // Only why the buttons may be grey — what the phrase is for is the row's own line.
        return !string.IsNullOrWhiteSpace(passwordProtectionErrorText)
            ? $"Не удалось узнать, задан ли пароль: {passwordProtectionErrorText}"
            : passwordProtectionSummary?.Enabled == true
                ? string.Empty
                : "Кнопки откроются, когда вы зададите пароль выше: без него фразу мог бы прочитать любой, кто сядет за компьютер.";
    }

    public static string BuildPasswordProtectionSummaryText(
        PasswordProtectionSummary? passwordProtectionSummary,
        string? passwordProtectionErrorText)
    {
        if (!string.IsNullOrWhiteSpace(passwordProtectionErrorText))
        {
            return $"Не удалось узнать, задан ли пароль: {passwordProtectionErrorText}";
        }

        if (passwordProtectionSummary is null || !passwordProtectionSummary.Enabled)
        {
            return "Не задан — Omni открывается без пароля. Пароль защищает от того, кто сядет за компьютер, но не от вредоносной программы.";
        }

        // Not the key-derivation parameters (Argon2 memory, iterations): they told the person nothing.
        // It is asked after «Выйти», not at every start: a profile left signed in opens by itself
        // (QA 2026-10-17 — the old «спрашивает его при запуске» promised what a restart did not do).
        return "Задан — ключ аккаунта и фраза под ним. Omni спросит пароль после «Выйти»; если не выходить, профиль на этом компьютере открывается сам.";
    }

    private static string BuildCompactContactStatus(
        ContactSnapshot contact,
        long nowUnixMs,
        IReadOnlySet<string> linkedAccountIds)
    {
        string relationshipState = contact.RelationshipState?.Trim().ToLowerInvariant() ?? string.Empty;
        if (relationshipState == "pending")
        {
            return BuildPendingContactStatus(contact);
        }

        if (relationshipState == "declined")
        {
            return "Отклонён";
        }

        if (relationshipState == "approved" &&
            !string.IsNullOrWhiteSpace(contact.RemoteAccountId) &&
            string.IsNullOrWhiteSpace(contact.RemoteInstanceId))
        {
            return "Сохранён локально";
        }

        if (!contact.HasMet)
        {
            return "Ещё не встречались";
        }

        return MapContactPresenceState(contact, nowUnixMs, linkedAccountIds);
    }

    private static string BuildContactBadgeText(ContactSnapshot contact)
    {
        List<string> parts = [];
        // Friend-proxy (Axis 2): surface the granted carriage consent so the current state is
        // visible on the card, not only inside the actions menu. See friend-proxy-by-consent.md.
        if (contact.FriendRelayConsent)
        {
            parts.Add("Маршрут через меня");
        }

        return string.Join(" • ", parts);
    }

    // The avatar presence dot: green = a LIVE mesh link is up right now, amber = recently online (no
    // live link), hidden otherwise. Only for a real met+approved contact (pending/declined/local-only
    // have no online state). `linkedAccountIds` = account-ids we currently hold a link to (from the
    // honest connection-state DTO) — so green means actually-reachable-now, NOT a stale announced
    // presence that lags a peer going offline (the green-dot-but-actually-reconnecting bug).
    internal static (Visibility Visibility, Brush Brush) MapContactPresenceDot(
        ContactSnapshot contact,
        long nowUnixMs,
        IReadOnlySet<string> linkedAccountIds)
        => MapContactPresenceDot(MapContactPresenceDotKind(contact, nowUnixMs, linkedAccountIds));

    internal static (Visibility Visibility, Brush Brush) MapContactPresenceDot(ContactPresenceDotKind kind)
        => kind switch
        {
            ContactPresenceDotKind.Live => (Visibility.Visible, PresenceBrush("SystemFillColorSuccessBrush")),
            ContactPresenceDotKind.Recent => (Visibility.Visible, PresenceBrush("SystemFillColorCautionBrush")),
            _ => (Visibility.Collapsed, PresenceBrush("SystemFillColorNeutralBrush")),
        };

    /// <summary>
    /// Тот же расчёт, но выдаёт РЕШЕНИЕ, а не кисть. Нужен спискам: их перерисовка отсекается по
    /// подписи, а подпись обязана покрывать все входы значения — иначе точка застывает на том, чем
    /// была в момент последней смены подписи. Кисть в подпись не положишь, решение — можно.
    /// </summary>
    internal static ContactPresenceDotKind MapContactPresenceDotKind(
        ContactSnapshot contact,
        long nowUnixMs,
        IReadOnlySet<string> linkedAccountIds)
    {
        bool presenceApplies =
            contact.RelationshipState.Equals("approved", StringComparison.OrdinalIgnoreCase) &&
            contact.HasMet &&
            !string.IsNullOrWhiteSpace(contact.RemoteInstanceId);
        if (!presenceApplies)
        {
            return ContactPresenceDotKind.None;
        }

        // GREEN strictly when we hold a live link now — the single source of truth for "online".
        bool linkedNow = contact.RemoteAccountId is string acct && linkedAccountIds.Contains(acct);
        if (linkedNow)
        {
            return ContactPresenceDotKind.Live;
        }

        // No live link → at most "recently online" (amber) by announced presence / recency, never green.
        long? lastActivity = contact.PresenceLastActivityAtUnixMs ?? contact.PresenceLastSeenAtUnixMs;
        long ageMs = lastActivity.HasValue ? nowUnixMs - lastActivity.Value : long.MaxValue;
        string presence = (contact.PresenceState ?? string.Empty).Trim().ToLowerInvariant();
        bool recent = presence is "online" or "recently" or "recent" || ageMs <= 5 * 60_000;
        return recent ? ContactPresenceDotKind.Recent : ContactPresenceDotKind.None;
    }

    // Resolve a theme brush by resource key, falling back to a supplied brush if it's missing.
    /// The window's effective theme (Light/Dark), kept by MainWindow.ThemeSettings. A plain
    /// `Application.Current.Resources` lookup answers for the APPLICATION theme, fixed at start
    /// from Windows — so with "Светлая" picked in Settings, code-resolved brushes (chat bubbles,
    /// presence dots) stayed dark on a light window (seen live 2026-10-10).
    internal static ElementTheme ActiveTheme { get; set; } = ElementTheme.Default;

    internal static Brush ResolveThemeBrush(string themeResourceKey, Brush fallback)
    {
        string? themeKey = ActiveTheme switch
        {
            ElementTheme.Light => "Light",
            ElementTheme.Dark => "Dark",
            _ => null,
        };
        if (themeKey is not null &&
            Application.Current.Resources.ThemeDictionaries.TryGetValue(themeKey, out object? dictionary) &&
            dictionary is ResourceDictionary themed &&
            themed.TryGetValue(themeResourceKey, out object? themedValue) &&
            themedValue is Brush themedBrush)
        {
            return themedBrush;
        }

        return Application.Current.Resources.TryGetValue(themeResourceKey, out object? value) && value is Brush brush
            ? brush
            : fallback;
    }

    private static Brush PresenceBrush(string themeResourceKey)
        => ResolveThemeBrush(themeResourceKey, new SolidColorBrush(Microsoft.UI.Colors.Gray));

    /// <summary>
    /// Текст присутствия — из ТОГО ЖЕ решения, что и точка. Раньше это были два независимых
    /// расчёта, и они расходились прямо на экране: подпись «В сети» рядом с оранжевой точкой
    /// (замечено пользователем 2026-08-14). Хуже того, текст «В сети» приходил вообще из другого
    /// слоя — из Rust'а (`app_chat_projection::contact_presence_label`), где выводился из
    /// ОБЪЯВЛЕННОГО присутствия и про живой линк не знал ничего.
    ///
    /// Инвариант теперь один: цвет и слово — две проекции одного `ContactPresenceDotKind`,
    /// посчитанного один раз за отрисовку. Разойтись им больше нечем.
    /// </summary>
    internal static string MapContactPresenceText(ContactSnapshot contact, ContactPresenceDotKind kind)
    {
        // «Never met» — про доверие, а не про присутствие: контакт может быть на связи прямо
        // сейчас и при этом непроверенным. Термин из модели доверия (см. CONTRIBUTING.md, Invariants).
        if (!contact.HasMet)
        {
            return "Ещё не встречались";
        }

        return kind switch
        {
            ContactPresenceDotKind.Live => "В сети",
            ContactPresenceDotKind.Recent => "Недавно в сети",
            _ => "Офлайн",
        };
    }

    private static string MapContactPresenceState(
        ContactSnapshot contact,
        long nowUnixMs,
        IReadOnlySet<string> linkedAccountIds)
        => MapContactPresenceText(
            contact,
            MapContactPresenceDotKind(contact, nowUnixMs, linkedAccountIds));

    public static string MapRelationshipState(string? relationshipState)
        => relationshipState?.Trim().ToLowerInvariant() switch
        {
            "approved" => "В контактах",
            "pending" => "Запрос",
            "declined" => "Отклонён",
            null or "" => "Неизвестно",
            _ => relationshipState,
        };

    private static string BuildPendingContactStatus(ContactSnapshot contact)
        => contact.PendingRequestDirection?.Trim().ToLowerInvariant() switch
        {
            "incoming" => "Входящий запрос",
            "outgoing" => "Запрос отправлен",
            _ => "Ожидает подтверждения",
        };

    private static bool IsAccountDialogAwaitingFirstConnection(
        ChatSecuritySummary? summary,
        ContactCardSnapshot? card)
    {
        if (summary is null || card is null)
        {
            return false;
        }

        return summary.DialogKey.StartsWith("dialog:account:", StringComparison.Ordinal) &&
               string.Equals(card.Contact.RelationshipState, "approved", StringComparison.OrdinalIgnoreCase) &&
               string.IsNullOrWhiteSpace(card.Contact.RemoteInstanceId);
    }
}


