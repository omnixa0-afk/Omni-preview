// What history sync between devices needs from storage (docs/decisions/group-history-sync.md,
// «1:1 при разделе сети»; account-recovery-from-friends.md, Б2): a chat's lines as both sides see
// them, where a line came from when a contact handed it over, and how a restored chat compared with
// the fingerprint its passport kept.

/// One line of a 1:1 chat as history sync compares it: whose it is, what it says, when its author
/// wrote it. Outgoing lines still in the queue are not history yet: the other side does not have them.
/// Messages with a file are not lines: a file goes with its offer and key, which history sync does
/// not carry yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatHistoryLine {
    pub id: String,
    pub outgoing: bool,
    pub body: String,
    pub created_at_unix_ms: i64,
}

/// How a chat restored from a contact's copy compared with the fingerprint in the passport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatHistoryCheck {
    /// Every line the passport counted is here and they hash the same: nothing added, changed or cut.
    Verified,
    /// Fewer lines than the passport counted came back so far.
    Incomplete,
    /// As many lines or more, and they do not hash the same: the copy differs from what we had.
    Mismatch,
}

impl ChatHistoryCheck {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Incomplete => "incomplete",
            Self::Mismatch => "mismatch",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "verified" => Some(Self::Verified),
            "incomplete" => Some(Self::Incomplete),
            "mismatch" => Some(Self::Mismatch),
            _ => None,
        }
    }
}

impl Storage {
    /// Whether a message with this id is already here (any chat, any direction).
    pub fn message_exists(&self, message_id: &str) -> OmniResult<bool> {
        let found: Option<i64> = self
            .connection
            .query_row("SELECT 1 FROM messages WHERE id = ?1", params![message_id], |row| row.get(0))
            .optional()
            .map_err(map_sqlite_error)?;
        Ok(found.is_some())
    }

    pub fn list_chat_history_lines(&self, chat_dialog_key: &str) -> OmniResult<Vec<ChatHistoryLine>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT id, direction, body, created_at_unix_ms FROM messages
                WHERE profile_id = ?1 AND dialog_key = ?2
                  AND (direction = 'incoming' OR (direction = 'outgoing' AND status IN ('sent', 'delivered')))
                ORDER BY created_at_unix_ms DESC, id DESC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(
                params![profile_id.0.as_str(), omni_types::normalize_dialog_key(chat_dialog_key)],
                |row| {
                    Ok(ChatHistoryLine {
                        id: row.get(0)?,
                        outgoing: row.get::<_, String>(1)? == "outgoing",
                        body: row.get(2)?,
                        created_at_unix_ms: row.get(3)?,
                    })
                },
            )
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)?;
        Ok(rows)
    }

    /// The line came from `source_account_id`'s copy of the chat, not from its author's own send.
    pub fn record_history_import(&self, message_id: &str, source_account_id: &AccountId, now_unix_ms: i64) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "
                INSERT INTO history_imports (profile_id, message_id, source_account_id, imported_at_unix_ms)
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(profile_id, message_id) DO NOTHING
                ",
                params![profile_id.0.as_str(), message_id, source_account_id.0.as_str(), now_unix_ms],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn history_import_source(&self, message_id: &str) -> OmniResult<Option<AccountId>> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .query_row(
                "SELECT source_account_id FROM history_imports WHERE profile_id = ?1 AND message_id = ?2",
                params![profile_id.0.as_str(), message_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map(|value| value.map(AccountId))
            .map_err(map_sqlite_error)
    }

    /// A sent line the contact's copy shows it has: it was delivered.
    pub fn mark_message_delivered_if_sent(&self, message_id: &str, now_unix_ms: i64) -> OmniResult<()> {
        self.connection
            .execute(
                "
                UPDATE messages SET status = 'delivered', remote_delivered_at_unix_ms = COALESCE(remote_delivered_at_unix_ms, ?2)
                WHERE id = ?1 AND direction = 'outgoing' AND status = 'sent'
                ",
                params![message_id, now_unix_ms],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn record_chat_history_check(&self, chat_account_id: &AccountId, check: ChatHistoryCheck, now_unix_ms: i64) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "
                INSERT INTO chat_history_checks (profile_id, chat_account_id, state, checked_at_unix_ms)
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(profile_id, chat_account_id) DO UPDATE SET
                    state = excluded.state, checked_at_unix_ms = excluded.checked_at_unix_ms
                ",
                params![profile_id.0.as_str(), chat_account_id.0.as_str(), check.as_str(), now_unix_ms],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn get_chat_history_check(&self, chat_account_id: &AccountId) -> OmniResult<Option<ChatHistoryCheck>> {
        let profile_id = self.require_active_profile_id()?;
        let state: Option<String> = self
            .connection
            .query_row(
                "SELECT state FROM chat_history_checks WHERE profile_id = ?1 AND chat_account_id = ?2",
                params![profile_id.0.as_str(), chat_account_id.0.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        Ok(state.as_deref().and_then(ChatHistoryCheck::parse))
    }
}
