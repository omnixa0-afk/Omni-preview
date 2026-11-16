// Deleted and pinned messages. A deletion leaves a tombstone: history sync, a newcomer's history and a
// late copy must not bring the line back. Pins are one row per message, the latest by its author's
// clock standing, so a late event cannot undo a newer one.

/// Tombstones an own device is told at once; older ones have already reached it.
const DELETIONS_TOLD_AT_ONCE: usize = 256;

/// Pins one conversation shows.
const PINS_SHOWN: usize = 100;

/// Deletions asked for lines not here yet, kept per conversation, and how long.
const DELETE_REQUESTS_KEPT: usize = 512;
const DELETE_REQUESTS_KEEP_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// How far ahead of our clock another device's clock may stamp a pin. Clocks of devices that keep time
/// differ by seconds; an hour leaves room for a badly set one.
pub const PIN_CLOCK_AHEAD_MS: i64 = 60 * 60 * 1000;

impl Storage {
    /// The line and all that hangs on it, here. Its dedup row stays: a resent copy stays unwanted.
    pub fn delete_message_here(&self, message_id: &str) -> OmniResult<()> {
        let profile_id = self.resolve_transition_profile_id()?;
        let mut file_paths: Vec<std::path::PathBuf> = Vec::new();
        {
            let mut statement = self
                .connection
                .prepare("SELECT local_blob_path, local_blob_managed, temp_download_path FROM message_attachments WHERE message_id = ?1 AND profile_id = ?2")
                .map_err(map_sqlite_error)?;
            let rows = statement
                .query_map(params![message_id, profile_id.0], |row| {
                    Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)? != 0, row.get::<_, Option<String>>(2)?))
                })
                .map_err(map_sqlite_error)?;
            for row in rows {
                let (blob, managed, temp) = row.map_err(map_sqlite_error)?;
                // A file of the person's own folder is theirs: only our managed copies go.
                file_paths.extend(blob.filter(|_| managed).map(std::path::PathBuf::from));
                file_paths.extend(temp.map(std::path::PathBuf::from));
            }
        }
        for sql in [
            "DELETE FROM message_attachments WHERE message_id = ?1 AND profile_id = ?2",
            "DELETE FROM outbound_queue WHERE message_id = ?1 AND profile_id = ?2",
            "DELETE FROM delivery_receipts WHERE message_id = ?1 AND profile_id = ?2",
            "DELETE FROM message_security_metadata WHERE message_id = ?1 AND profile_id = ?2",
            "DELETE FROM group_signed_lines WHERE message_id = ?1 AND profile_id = ?2",
            "DELETE FROM group_message_meta WHERE message_id = ?1 AND profile_id = ?2",
            "DELETE FROM messages WHERE id = ?1 AND profile_id = ?2",
        ] {
            self.connection.execute(sql, params![message_id, profile_id.0]).map_err(map_sqlite_error)?;
        }
        // After the rows: a managed copy is shared by every message of the same content, and goes
        // only with the last of them.
        self.delete_attachment_files(file_paths)?;
        Ok(())
    }

    /// `unsent`: for everyone, and no link has taken it yet. The changed mark lets an open conversation
    /// see a line go, whatever the author's clock says.
    pub fn record_message_deletion(&self, message_id: &str, dialog_key: &str, at_unix_ms: i64, unsent: bool) -> OmniResult<()> {
        let profile_id = self.resolve_transition_profile_id()?;
        self.connection
            .execute(
                "INSERT INTO message_deletions (profile_id, message_id, dialog_key, at_unix_ms, unsent, changed)
                 VALUES (?5, ?1, ?2, ?3, ?4, COALESCE((SELECT MAX(changed) FROM message_deletions WHERE profile_id = ?5), 0) + 1)
                 ON CONFLICT (profile_id, message_id) DO UPDATE SET unsent = MAX(unsent, excluded.unsent)",
                params![message_id, dialog_key, at_unix_ms, unsent as i64, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        self.bump_message_actions_mark()
    }

    /// Someone asked to delete a line we do not have yet: kept with who asked (`requester_pub` — a
    /// group member's card key, empty for the contact of a 1:1 chat; `requester_account`), to be
    /// checked against the line's author when it comes ([`Self::take_message_delete_requests`]). A
    /// conversation keeps the newest [`DELETE_REQUESTS_KEPT`], none older than
    /// [`DELETE_REQUESTS_KEEP_MS`] — anyone in it can make up ids.
    pub fn record_message_delete_request(
        &self,
        message_id: &str,
        dialog_key: &str,
        requester_pub: &str,
        requester_account: &str,
        at_unix_ms: i64,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        let profile_id = self.resolve_transition_profile_id()?;
        self.connection
            .execute(
                "INSERT INTO message_delete_requests (profile_id, message_id, dialog_key, requester_pub, requester_account, at_unix_ms, received_at_unix_ms)
                 VALUES (?7, ?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT DO NOTHING",
                params![message_id, dialog_key, requester_pub, requester_account, at_unix_ms, now_unix_ms, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "DELETE FROM message_delete_requests WHERE profile_id = ?4 AND (received_at_unix_ms < ?1
                    OR (dialog_key = ?2 AND rowid NOT IN (
                        SELECT rowid FROM message_delete_requests WHERE profile_id = ?4 AND dialog_key = ?2
                        ORDER BY received_at_unix_ms DESC LIMIT ?3)))",
                params![now_unix_ms.saturating_sub(DELETE_REQUESTS_KEEP_MS), dialog_key, DELETE_REQUESTS_KEPT as i64, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// The deletions asked for `message_id` before it came, taken away: `(dialog_key, requester_pub,
    /// requester_account, at_unix_ms)` each.
    pub fn take_message_delete_requests(&self, message_id: &str) -> OmniResult<Vec<(String, String, String, i64)>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let mut statement = self
            .connection
            .prepare("SELECT dialog_key, requester_pub, requester_account, at_unix_ms FROM message_delete_requests WHERE message_id = ?1 AND profile_id = ?2")
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![message_id, profile_id.0], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)?;
        if !rows.is_empty() {
            self.connection
                .execute("DELETE FROM message_delete_requests WHERE message_id = ?1 AND profile_id = ?2", params![message_id, profile_id.0])
                .map_err(map_sqlite_error)?;
        }
        Ok(rows)
    }

    pub fn message_is_deleted(&self, message_id: &str) -> OmniResult<bool> {
        let profile_id = self.resolve_transition_profile_id()?;
        Ok(self
            .connection
            .query_row("SELECT 1 FROM message_deletions WHERE message_id = ?1 AND profile_id = ?2", params![message_id, profile_id.0], |_| Ok(()))
            .optional()
            .map_err(map_sqlite_error)?
            .is_some())
    }

    pub fn unsent_message_deletions(&self, dialog_key: &str) -> OmniResult<Vec<omni_types::MessageDeleteV1>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let mut statement = self
            .connection
            .prepare("SELECT message_id, at_unix_ms FROM message_deletions WHERE unsent = 1 AND profile_id = ?2 AND dialog_key = ?1 ORDER BY at_unix_ms")
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![dialog_key, profile_id.0], |row| Ok(omni_types::MessageDeleteV1 { message_id: row.get(0)?, at_unix_ms: row.get(1)? }))
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    pub fn mark_message_deletion_sent(&self, message_id: &str) -> OmniResult<()> {
        let profile_id = self.resolve_transition_profile_id()?;
        self.connection
            .execute("UPDATE message_deletions SET unsent = 0 WHERE message_id = ?1 AND profile_id = ?2", params![message_id, profile_id.0])
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// The newest 1:1 tombstones for our own devices, with the highest mark among them.
    pub fn recent_chat_deletions(&self) -> OmniResult<(i64, Vec<String>)> {
        let profile_id = self.resolve_transition_profile_id()?;
        let mut statement = self
            .connection
            .prepare("SELECT message_id, changed FROM message_deletions WHERE profile_id = ?2 AND dialog_key NOT LIKE 'group:%' ORDER BY changed DESC LIMIT ?1")
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![DELETIONS_TOLD_AT_ONCE as i64, profile_id.0], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
            .map_err(map_sqlite_error)?;
        let rows = rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)?;
        let newest = rows.first().map_or(0, |(_, changed)| *changed);
        Ok((newest, rows.into_iter().map(|(id, _)| id).collect()))
    }

    /// The newest lines deleted in a conversation: what an open view may still be showing.
    pub fn recent_deletions_in_dialog(&self, dialog_key: &str) -> OmniResult<Vec<String>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let mut statement = self
            .connection
            .prepare("SELECT message_id FROM message_deletions WHERE profile_id = ?3 AND dialog_key = ?1 ORDER BY changed DESC LIMIT ?2")
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![dialog_key, DELETIONS_TOLD_AT_ONCE as i64, profile_id.0], |row| row.get::<_, String>(0))
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    /// Who wrote a group line, as the group's card named it.
    pub fn group_line_sender_pub(&self, message_id: &str) -> OmniResult<Option<String>> {
        self.connection
            .query_row("SELECT sender_pub FROM group_message_meta WHERE message_id = ?1 AND profile_id = ?2", params![message_id, self.resolve_transition_profile_id()?.0], |row| row.get(0))
            .optional()
            .map_err(map_sqlite_error)
            .map(Option::flatten)
    }

    /// Pinned or unpinned here, by the person: it always takes — after the pin it replaces even when
    /// the other side's clock runs a little ahead, at once over one stamped beyond any clock
    /// ([`PIN_CLOCK_AHEAD_MS`]). Returns the time it was stamped with (`None`: not a message of this
    /// conversation), which the event carries.
    pub fn pin_message_here(&self, message_id: &str, dialog_key: &str, pinned: bool, now_unix_ms: i64, unsent: bool) -> OmniResult<Option<i64>> {
        let at = self.pin_time_for(message_id, now_unix_ms)?;
        Ok(self.set_message_pin(message_id, dialog_key, pinned, at, unsent, now_unix_ms)?.then_some(at))
    }

    /// The time a pin of ours made now carries: past the one that stands, so that it stands.
    pub fn pin_time_for(&self, message_id: &str, now_unix_ms: i64) -> OmniResult<i64> {
        let previous: Option<i64> = self
            .connection
            .query_row("SELECT at_unix_ms FROM message_pins WHERE message_id = ?1 AND profile_id = ?2", params![message_id, self.resolve_transition_profile_id()?.0], |row| row.get(0))
            .optional()
            .map_err(map_sqlite_error)?;
        let at = match previous {
            Some(previous) if previous <= now_unix_ms.saturating_add(PIN_CLOCK_AHEAD_MS) => now_unix_ms.max(previous + 1),
            _ => now_unix_ms,
        };
        Ok(at)
    }

    /// A message pinned or unpinned; the latest by its author's clock stands. Only a message of this
    /// conversation can be pinned in it. A pin stamped more than [`PIN_CLOCK_AHEAD_MS`] past `now` is
    /// refused, and one already stored that far ahead gives way to any: `i64::MAX` from a member used to
    /// stand for good — no later pin or unpin could beat it (audit 2026-11-09, F08).
    pub fn set_message_pin(&self, message_id: &str, dialog_key: &str, pinned: bool, at_unix_ms: i64, unsent: bool, now_unix_ms: i64) -> OmniResult<bool> {
        let ceiling = now_unix_ms.saturating_add(PIN_CLOCK_AHEAD_MS);
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
                "INSERT INTO message_pins (message_id, dialog_key, pinned, at_unix_ms, unsent, profile_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?7)
                 ON CONFLICT (profile_id, message_id) DO UPDATE
                    SET pinned = excluded.pinned, at_unix_ms = excluded.at_unix_ms, unsent = excluded.unsent
                    WHERE excluded.at_unix_ms > message_pins.at_unix_ms OR message_pins.at_unix_ms > ?6",
                params![message_id, dialog_key, pinned as i64, at_unix_ms, unsent as i64, ceiling, self.resolve_transition_profile_id()?.0],
            )
            .map_err(map_sqlite_error)?;
        if changed > 0 {
            self.bump_message_actions_mark()?;
        }
        Ok(changed > 0)
    }

    pub fn message_is_pinned(&self, message_id: &str) -> OmniResult<bool> {
        Ok(self
            .connection
            .query_row(
                "SELECT 1 FROM message_pins p JOIN messages m ON m.id = p.message_id AND m.profile_id = p.profile_id
                 WHERE p.message_id = ?1 AND p.pinned = 1 AND m.profile_id = ?2",
                params![message_id, self.resolve_transition_profile_id()?.0],
                |_| Ok(()),
            )
            .optional()
            .map_err(map_sqlite_error)?
            .is_some())
    }

    /// A conversation's pinned messages, the latest pinned first, each as a quote shows it.
    pub fn pinned_messages(&self, dialog_key: &str) -> OmniResult<Vec<omni_types::MessageReplyPreview>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT target.id, target.direction, target.body,
                        (SELECT CASE json_extract(a.presentation_json, '$.kind')
                                    WHEN 'voice' THEN 'Голосовое сообщение'
                                    WHEN 'video_note' THEN 'Видеосообщение'
                                    WHEN 'photo' THEN 'Фото'
                                    ELSE a.file_name END
                         FROM message_attachments a WHERE a.message_id = target.id AND a.profile_id = target.profile_id),
                        (SELECT g.sender_name FROM group_message_meta g WHERE g.message_id = target.id AND g.profile_id = target.profile_id)
                 FROM message_pins p JOIN messages target ON target.id = p.message_id AND target.profile_id = p.profile_id
                 WHERE p.dialog_key = ?1 AND p.pinned = 1 AND target.profile_id = ?3
                 ORDER BY p.at_unix_ms DESC LIMIT ?2",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![dialog_key, PINS_SHOWN as i64, self.resolve_transition_profile_id()?.0], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .map_err(map_sqlite_error)?;
        let mut views = Vec::new();
        for row in rows {
            let (id, direction, body, file_name, author) = row.map_err(map_sqlite_error)?;
            let body = body.trim();
            let text = if body.is_empty() { file_name.unwrap_or_default() } else { body.chars().take(REPLY_PREVIEW_MAX_CHARS).collect() };
            views.push(omni_types::MessageReplyPreview {
                message_id: id,
                from_me: matches!(parse_direction(&direction)?, ChatDirection::Outgoing),
                author: author.unwrap_or_default(),
                text,
            });
        }
        Ok(views)
    }

    pub fn unsent_message_pins(&self, dialog_key: &str) -> OmniResult<Vec<omni_types::MessagePinV1>> {
        let mut statement = self
            .connection
            .prepare("SELECT message_id, pinned, at_unix_ms FROM message_pins WHERE unsent = 1 AND dialog_key = ?1 AND profile_id = ?2 ORDER BY at_unix_ms")
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![dialog_key, self.resolve_transition_profile_id()?.0], |row| {
                Ok(omni_types::MessagePinV1 { message_id: row.get(0)?, pinned: row.get::<_, i64>(1)? != 0, at_unix_ms: row.get(2)? })
            })
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    pub fn mark_message_pin_sent(&self, message_id: &str) -> OmniResult<()> {
        self.connection
            .execute("UPDATE message_pins SET unsent = 0 WHERE message_id = ?1 AND profile_id = ?2", params![message_id, self.resolve_transition_profile_id()?.0])
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    // An action on a line changes no line's time (a deletion leaves none): this mark is what a view's refresh
    // summary sees change, and it only grows.
    fn bump_message_actions_mark(&self) -> OmniResult<()> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
        self.connection
            .execute(
                "INSERT INTO message_actions_mark (only_row, changed_at_unix_ms) VALUES (1, ?1)
                 ON CONFLICT (only_row) DO UPDATE SET changed_at_unix_ms = MAX(changed_at_unix_ms + 1, excluded.changed_at_unix_ms)",
                params![now],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn message_actions_mark(&self) -> OmniResult<Option<i64>> {
        self.connection
            .query_row("SELECT changed_at_unix_ms FROM message_actions_mark WHERE only_row = 1", [], |row| row.get(0))
            .optional()
            .map_err(map_sqlite_error)
    }
}
