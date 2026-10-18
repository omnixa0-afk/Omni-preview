// What this device has read of each chat (docs/decisions/unread-counters.md): one mark per dialog,
// the author time of the newest line read. A line is unread when it is incoming, not a group's own
// system line, and newer than the mark. The mark only moves forward.

impl Storage {
    /// The chat was read up to its newest line: the shell calls it while the chat is on screen.
    pub fn mark_dialog_read(&self, dialog_key: &str) -> OmniResult<()> {
        let profile_id = self.resolve_transition_profile_id()?;
        let dialog_key = normalize_dialog_key(dialog_key);
        let newest: Option<i64> = self
            .connection
            .query_row(
                "SELECT MAX(created_at_unix_ms) FROM messages WHERE profile_id = ?1 AND dialog_key = ?2",
                params![profile_id.0, dialog_key],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        match newest {
            Some(newest) => self.mark_dialog_read_through(&dialog_key, newest),
            None => Ok(()),
        }
    }

    /// Lines up to `through_unix_ms` count as read: our own line (we saw the chat to write it), and
    /// history handed over by a sync — old talk coming back, not news.
    pub fn mark_dialog_read_through(&self, dialog_key: &str, through_unix_ms: i64) -> OmniResult<()> {
        let profile_id = self.resolve_transition_profile_id()?;
        let dialog_key = normalize_dialog_key(dialog_key);
        // `updated_at` is the chat list's change signal, so it must move on every update even when two
        // land in the same millisecond: never below the profile's last value + 1.
        self.connection
            .execute(
                "
                INSERT INTO dialog_read_marks (profile_id, dialog_key, read_through_unix_ms, updated_at_unix_ms)
                VALUES (?1, ?2, ?3, MAX(?4, COALESCE(
                    (SELECT MAX(updated_at_unix_ms) FROM dialog_read_marks WHERE profile_id = ?1), 0) + 1))
                ON CONFLICT(profile_id, dialog_key) DO UPDATE SET
                    read_through_unix_ms = excluded.read_through_unix_ms,
                    updated_at_unix_ms = excluded.updated_at_unix_ms
                WHERE excluded.read_through_unix_ms > dialog_read_marks.read_through_unix_ms
                ",
                params![profile_id.0, dialog_key, through_unix_ms, current_unix_ms()],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// Unread lines of one chat. Walks the (profile, dialog, created) index from the mark, so it costs
    /// the unread lines, not the history.
    pub fn count_unread_in_dialog(&self, dialog_key: &str) -> OmniResult<u32> {
        let profile_id = self.resolve_transition_profile_id()?;
        let dialog_key = normalize_dialog_key(dialog_key);
        let count: i64 = self
            .connection
            .prepare_cached(
                "
                SELECT COUNT(*)
                FROM messages m
                WHERE m.profile_id = ?1
                  AND m.dialog_key = ?2
                  AND m.created_at_unix_ms > COALESCE(
                      (SELECT read_through_unix_ms FROM dialog_read_marks r
                       WHERE r.profile_id = ?1 AND r.dialog_key = ?2),
                      0)
                  AND m.direction = 'incoming'
                  AND NOT EXISTS (
                      SELECT 1 FROM group_message_meta g
                      WHERE g.profile_id = ?1 AND g.message_id = m.id AND g.kind = 'system'
                  )
                ",
            )
            .map_err(map_sqlite_error)?
            .query_row(params![profile_id.0, dialog_key], |row| row.get(0))
            .map_err(map_sqlite_error)?;
        Ok(count.max(0) as u32)
    }

    /// When a mark last moved — part of the chat list's refresh signature, so reading a chat on
    /// screen clears its counter in the list.
    pub fn last_read_mark_updated_at_unix_ms(&self) -> OmniResult<Option<i64>> {
        let profile_id = self.resolve_transition_profile_id()?;
        self.connection
            .query_row(
                "SELECT MAX(updated_at_unix_ms) FROM dialog_read_marks WHERE profile_id = ?1",
                params![profile_id.0],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)
    }
}
