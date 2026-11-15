// What a message answers, and the answered message as a quote shows it. One table for 1:1 chats and
// groups: both keep their lines in `messages`.

/// A quote shows this many characters of the answered message.
const REPLY_PREVIEW_MAX_CHARS: usize = 120;

impl Storage {
    /// Our own answer, before its message is written. An answer to something that is not in this
    /// conversation goes as a plain message.
    pub fn remember_message_reply(&self, message_id: &str, dialog_key: &str, reply_to: Option<&str>) -> OmniResult<()> {
        let Some(reply_to) = reply_to else {
            return Ok(());
        };
        let profile_id = self.resolve_transition_profile_id()?;
        let here = self
            .connection
            .query_row(
                "SELECT 1 FROM messages WHERE id = ?1 AND dialog_key = ?2 AND profile_id = ?3",
                params![reply_to, dialog_key, profile_id.0],
                |_| Ok(()),
            )
            .optional()
            .map_err(map_sqlite_error)?
            .is_some();
        if here {
            self.remember_received_message_reply(message_id, Some(reply_to))?;
        }
        Ok(())
    }

    /// What a received message says it answers. Whether that message is in the same conversation is
    /// checked when the quote is read (`message_reply_preview`); the first word on a message stands,
    /// as its first copy does.
    pub fn remember_received_message_reply(&self, message_id: &str, reply_to: Option<&str>) -> OmniResult<()> {
        let Some(reply_to) = reply_to else {
            return Ok(());
        };
        self.connection
            .execute(
                "INSERT OR IGNORE INTO message_replies (message_id, reply_to, profile_id) VALUES (?1, ?2, ?3)",
                params![message_id, reply_to, self.resolve_transition_profile_id()?.0],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// The accounts a group line names. The first word on a line stands, as with its reply.
    pub fn remember_message_mentions(&self, message_id: &str, accounts: &[String]) -> OmniResult<()> {
        if accounts.is_empty() {
            return Ok(());
        }
        let accounts = serde_json::to_string(accounts).map_err(|e| OmniError::Internal(e.to_string()))?;
        self.connection
            .execute(
                "INSERT OR IGNORE INTO message_mentions (message_id, accounts_json, profile_id) VALUES (?1, ?2, ?3)",
                params![message_id, accounts, self.resolve_transition_profile_id()?.0],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn message_mentions(&self, message_id: &str) -> OmniResult<Vec<String>> {
        let accounts: Option<String> = self
            .connection
            .query_row(
                "SELECT accounts_json FROM message_mentions WHERE message_id = ?1 AND profile_id = ?2",
                params![message_id, self.resolve_transition_profile_id()?.0],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        Ok(accounts.and_then(|json| serde_json::from_str(&json).ok()).unwrap_or_default())
    }

    /// A person's reaction; the latest by its author's clock stands. A message of another conversation
    /// is not reacted to, and the message shows the change as any other (`updated_at_unix_ms`). Like a
    /// pin, one stamped more than `PIN_CLOCK_AHEAD_MS` past `now` is refused and one stored that far
    /// ahead gives way: `i64::MAX` used to lock the person's reaction and the message's update time.
    #[allow(clippy::too_many_arguments)]
    pub fn set_message_reaction(&self, message_id: &str, dialog_key: &str, reactor: &str, mine: bool, emoji: &str, at_unix_ms: i64, unsent: bool, now_unix_ms: i64) -> OmniResult<bool> {
        let ceiling = now_unix_ms.saturating_add(crate::PIN_CLOCK_AHEAD_MS);
        if at_unix_ms > ceiling {
            return Ok(false);
        }
        let here = self
            .connection
            .query_row(
                "SELECT 1 FROM messages WHERE id = ?1 AND dialog_key = ?2 AND profile_id = ?3",
                params![message_id, dialog_key, self.resolve_transition_profile_id()?.0],
                |_| Ok(()),
            )
            .optional()
            .map_err(map_sqlite_error)?
            .is_some();
        if !here {
            return Ok(false);
        }
        let changed = self
            .connection
            .execute(
                "INSERT INTO message_reactions (message_id, reactor, mine, emoji, at_unix_ms, unsent, profile_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?8)
                 ON CONFLICT (profile_id, message_id, reactor) DO UPDATE
                    SET emoji = excluded.emoji, at_unix_ms = excluded.at_unix_ms, unsent = excluded.unsent
                    WHERE excluded.at_unix_ms > message_reactions.at_unix_ms OR message_reactions.at_unix_ms > ?7",
                params![message_id, reactor, mine as i64, emoji, at_unix_ms, unsent as i64, ceiling, self.resolve_transition_profile_id()?.0],
            )
            .map_err(map_sqlite_error)?;
        if changed > 0 {
            self.connection
                .execute(
                    "UPDATE messages SET updated_at_unix_ms = MAX(updated_at_unix_ms + 1, ?2) WHERE id = ?1 AND profile_id = ?3",
                    params![message_id, at_unix_ms, self.resolve_transition_profile_id()?.0],
                )
                .map_err(map_sqlite_error)?;
            self.bump_message_actions_mark()?;
        }
        Ok(changed > 0)
    }

    /// The emoji on a message, the most used first; `who` holds accounts.
    pub fn message_reactions(&self, message_id: &str) -> OmniResult<Vec<omni_types::MessageReactionView>> {
        let mut statement = self
            .connection
            .prepare_cached("SELECT emoji, reactor, mine FROM message_reactions WHERE profile_id = ?2 AND message_id = ?1 AND emoji <> '' ORDER BY at_unix_ms")
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![message_id, self.resolve_transition_profile_id()?.0], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)? != 0)))
            .map_err(map_sqlite_error)?;
        let mut views: Vec<omni_types::MessageReactionView> = Vec::new();
        for row in rows {
            let (emoji, reactor, mine) = row.map_err(map_sqlite_error)?;
            match views.iter_mut().find(|v| v.emoji == emoji) {
                Some(view) => {
                    view.count += 1;
                    view.mine |= mine;
                    view.who.push(reactor);
                }
                None => views.push(omni_types::MessageReactionView { emoji, count: 1, mine, who: vec![reactor] }),
            }
        }
        views.sort_by_key(|v| std::cmp::Reverse(v.count));
        Ok(views)
    }

    /// Our reactions in a 1:1 dialog no link has taken yet: (reactor, reaction).
    pub fn unsent_reactions(&self, dialog_key: &str) -> OmniResult<Vec<(String, omni_types::ReactionV1)>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT r.reactor, r.message_id, r.emoji, r.at_unix_ms
                 FROM message_reactions r JOIN messages m ON m.id = r.message_id AND m.profile_id = r.profile_id
                 WHERE r.unsent = 1 AND m.dialog_key = ?1 AND m.profile_id = ?2
                 ORDER BY r.at_unix_ms",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![dialog_key, self.resolve_transition_profile_id()?.0], |row| {
                Ok((row.get::<_, String>(0)?, omni_types::ReactionV1 { message_id: row.get(1)?, emoji: row.get(2)?, at_unix_ms: row.get(3)? }))
            })
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    pub fn mark_reaction_sent(&self, message_id: &str, reactor: &str) -> OmniResult<()> {
        self.connection
            .execute(
                "UPDATE message_reactions SET unsent = 0 WHERE message_id = ?1 AND reactor = ?2 AND profile_id = ?3",
                params![message_id, reactor, self.resolve_transition_profile_id()?.0],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn message_reply_to(&self, message_id: &str) -> OmniResult<Option<String>> {
        self.connection
            .query_row(
                "SELECT reply_to FROM message_replies WHERE message_id = ?1 AND profile_id = ?2",
                params![message_id, self.resolve_transition_profile_id()?.0],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    /// The quote of `message_id`, read from this device's copy of the answered message: none when it
    /// is not here or is in another conversation (a peer must not be able to quote those).
    pub fn message_reply_preview(&self, message_id: &str, dialog_key: &str) -> OmniResult<Option<omni_types::MessageReplyPreview>> {
        let found = self
            .connection
            .query_row(
                "SELECT target.id, target.direction, target.body,
                        (SELECT CASE json_extract(a.presentation_json, '$.kind')
                                    WHEN 'voice' THEN 'Голосовое сообщение'
                                    WHEN 'video_note' THEN 'Видеосообщение'
                                    WHEN 'photo' THEN 'Фото'
                                    ELSE a.file_name END
                         FROM message_attachments a WHERE a.message_id = target.id AND a.profile_id = target.profile_id),
                        (SELECT g.sender_name FROM group_message_meta g WHERE g.message_id = target.id AND g.profile_id = target.profile_id)
                 FROM message_replies r
                 JOIN messages target ON target.id = r.reply_to AND target.profile_id = r.profile_id
                 WHERE r.message_id = ?1 AND target.dialog_key = ?2 AND target.profile_id = ?3",
                params![message_id, dialog_key, self.resolve_transition_profile_id()?.0],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let Some((id, direction, body, file_name, author)) = found else {
            return Ok(None);
        };
        let body = body.trim();
        let text = if body.is_empty() { file_name.unwrap_or_default() } else { body.chars().take(REPLY_PREVIEW_MAX_CHARS).collect() };
        Ok(Some(omni_types::MessageReplyPreview {
            message_id: id,
            from_me: matches!(parse_direction(&direction)?, ChatDirection::Outgoing),
            author: author.unwrap_or_default(),
            text,
        }))
    }
}
