impl Storage {
    pub fn healthcheck(&self) -> OmniResult<()> {
        let ping: i64 = self
            .connection
            .query_row("SELECT 1", [], |row| row.get(0))
            .map_err(map_sqlite_error)?;

        if ping != 1 {
            return Err(OmniError::Storage(
                "unexpected sqlite healthcheck result".to_string(),
            ));
        }

        self.connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'messages'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(map_sqlite_error)?;

        Ok(())
    }

    pub fn get_or_create_local_instance_id(&self) -> OmniResult<String> {
        if let Some(existing) = self.get_app_meta(LOCAL_INSTANCE_ID_META_KEY)? {
            return Ok(existing);
        }

        let instance_id = generate_local_instance_id();
        self.insert_app_meta_if_missing(LOCAL_INSTANCE_ID_META_KEY, &instance_id)?;

        self.get_app_meta(LOCAL_INSTANCE_ID_META_KEY)?
            .ok_or_else(|| OmniError::Storage("failed to persist local_instance_id".to_string()))
    }

    pub fn get_app_meta(&self, key: &str) -> OmniResult<Option<String>> {
        self.connection
            .query_row(
                "SELECT value FROM app_meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    pub fn upsert_app_meta(&self, key: &str, value: &str) -> OmniResult<()> {
        self.connection
            .execute(
                "
                INSERT INTO app_meta (key, value)
                VALUES (?1, ?2)
                ON CONFLICT(key) DO UPDATE SET value = excluded.value
                ",
                params![key, value],
            )
            .map_err(map_sqlite_error)?;

        Ok(())
    }

    pub fn insert_outgoing_message(
        &self,
        id: &str,
        dialog_key: &str,
        body: &str,
        status: MessageStatus,
        created_at_unix_ms: i64,
    ) -> OmniResult<ChatMessageRecord> {
        self.insert_outgoing_row(id, dialog_key, body, status, created_at_unix_ms)?;
        self.upsert_outbound_queue(
            id,
            &normalize_dialog_key(dialog_key),
            status,
            0,
            None,
            None,
            None,
            None,
            created_at_unix_ms,
            created_at_unix_ms,
        )?;
        self.get_message_by_id(id)
    }

    /// Our own line in a GROUP thread. Not queued: a group message is delivered by the group feed
    /// (MLS + durable mesh), not by the 1:1 retry queue — which would otherwise wait for a 1:1
    /// receipt that never comes and flip it to "retrying".
    pub fn insert_group_outgoing_message(
        &self,
        id: &str,
        dialog_key: &str,
        body: &str,
        created_at_unix_ms: i64,
    ) -> OmniResult<ChatMessageRecord> {
        self.insert_outgoing_row(id, dialog_key, body, MessageStatus::Sent, created_at_unix_ms)?;
        self.get_message_by_id(id)
    }

    fn insert_outgoing_row(
        &self,
        id: &str,
        dialog_key: &str,
        body: &str,
        status: MessageStatus,
        created_at_unix_ms: i64,
    ) -> OmniResult<()> {
        let profile_id = self.resolve_transition_profile_id()?;
        let dialog_key = normalize_dialog_key(dialog_key);

        self.connection
            .execute(
                "
                INSERT INTO messages (
                    id,
                    dialog_key,
                    direction,
                    body,
                    status,
                    created_at_unix_ms,
                    remote_delivered_at_unix_ms,
                    updated_at_unix_ms,
                    profile_id
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8)
                ",
                params![
                    id,
                    dialog_key,
                    direction_to_db(ChatDirection::Outgoing),
                    body,
                    status_to_db(status),
                    created_at_unix_ms,
                    created_at_unix_ms,
                    profile_id.0,
                ],
            )
            .map_err(map_sqlite_error)?;
        // Writing in a chat — here or on another of our devices — means what came before was seen.
        self.mark_dialog_read_through(&dialog_key, created_at_unix_ms)
    }

    pub fn insert_incoming_message(
        &self,
        id: &str,
        dialog_key: &str,
        body: &str,
        created_at_unix_ms: i64,
    ) -> OmniResult<ChatMessageRecord> {
        let profile_id = self.resolve_transition_profile_id()?;
        // Arrives on a device's dialog, belongs to the contact's chat (multi-device-equal-nodes.md).
        let dialog_key = self.chat_dialog_key_for(dialog_key)?;

        self.connection
            .execute(
                "
                INSERT OR IGNORE INTO messages (
                    id,
                    dialog_key,
                    direction,
                    body,
                    status,
                    created_at_unix_ms,
                    remote_delivered_at_unix_ms,
                    updated_at_unix_ms,
                    profile_id
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8)
                ",
                params![
                    id,
                    dialog_key,
                    direction_to_db(ChatDirection::Incoming),
                    body,
                    status_to_db(MessageStatus::Delivered),
                    created_at_unix_ms,
                    created_at_unix_ms,
                    profile_id.0,
                ],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                INSERT OR IGNORE INTO inbound_dedup (message_id, dialog_key, first_seen_unix_ms, profile_id)
                VALUES (?1, ?2, ?3, ?4)
                ",
                params![id, dialog_key, created_at_unix_ms, profile_id.0],
            )
            .map_err(map_sqlite_error)?;

        self.get_message_by_id(id)
    }

    pub fn list_messages_for_dialog(
        &self,
        dialog_key: &str,
    ) -> OmniResult<Vec<ChatMessageRecord>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let trimmed = dialog_key.trim();
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT
                    id,
                    dialog_key,
                    direction,
                    body,
                    status,
                    remote_delivered_at_unix_ms,
                    created_at_unix_ms,
                    updated_at_unix_ms
                FROM messages
                WHERE profile_id = ?2 AND dialog_key = ?1
                ORDER BY created_at_unix_ms ASC, id ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(
                params![trimmed, profile_id.0],
                map_message_row,
            )
            .map_err(map_sqlite_error)?;

        let mut messages = Vec::new();
        for row in rows {
            messages.push(self.attach_runtime_info_to_message(row.map_err(map_sqlite_error)?)?);
        }

        if trimmed.starts_with(omni_types::DIALOG_KEY_INSTANCE_PREFIX)
            || trimmed.starts_with(omni_types::DIALOG_KEY_ACCOUNT_PREFIX)
        {
            messages.extend(self.list_call_history_messages_for_dialog(trimmed)?);
            messages.sort_by(|left, right| {
                left.created_at_unix_ms
                    .cmp(&right.created_at_unix_ms)
                    .then_with(|| left.id.cmp(&right.id))
            });
        }

        Ok(messages)
    }

    pub fn list_messages_page_for_dialog(
        &self,
        dialog_key: &str,
        before_created_at_unix_ms: Option<i64>,
        before_message_id: Option<&str>,
        limit: usize,
    ) -> OmniResult<ListMessagesPageResponse> {
        if before_created_at_unix_ms.is_some() != before_message_id.is_some() {
            return Err(OmniError::InvalidInput(
                "before cursor must include both before_created_at_unix_ms and before_message_id"
                    .to_string(),
            ));
        }

        if limit == 0 {
            return Err(OmniError::InvalidInput(
                "limit must be greater than zero".to_string(),
            ));
        }

        let normalized_dialog_key = normalize_dialog_key(dialog_key);
        let before_message_id = before_message_id.unwrap_or_default();
        let filtered_messages = self
            .list_messages_for_dialog(&normalized_dialog_key)?
            .into_iter()
            .filter(|message| {
                before_created_at_unix_ms.is_none_or(|cursor_created_at_unix_ms| {
                    message.created_at_unix_ms < cursor_created_at_unix_ms
                        || (message.created_at_unix_ms == cursor_created_at_unix_ms
                            && message.id.as_str() < before_message_id)
                })
            })
            .collect::<Vec<_>>();

        let mut newest_first = filtered_messages
            .into_iter()
            .rev()
            .take(limit + 1)
            .collect::<Vec<_>>();
        let has_older = newest_first.len() > limit;
        if has_older {
            newest_first.pop();
        }
        newest_first.reverse();
        let messages = newest_first;

        Ok(ListMessagesPageResponse {
            dialog_key: normalized_dialog_key,
            oldest_loaded_created_at_unix_ms: messages.first().map(|message| message.created_at_unix_ms),
            oldest_loaded_message_id: messages.first().map(|message| message.id.clone()),
            has_older,
            messages,
        })
    }

    pub fn list_messages_for_dialog_after_cursor(
        &self,
        dialog_key: &str,
        after_message_updated_at_unix_ms: Option<i64>,
        after_message_id: Option<&str>,
        after_attachment_updated_at_unix_ms: Option<i64>,
        after_attachment_message_id: Option<&str>,
    ) -> OmniResult<Vec<ChatMessageRecord>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let normalized_dialog_key = normalize_dialog_key(dialog_key);
        let mut changed_message_ids = std::collections::BTreeSet::<String>::new();

        if let Some(after_updated_at_unix_ms) = after_message_updated_at_unix_ms {
            let after_message_id = after_message_id.unwrap_or_default();
            let mut statement = self
                .connection
                .prepare(
                    "
                    SELECT id
                    FROM messages
                    WHERE dialog_key = ?1
                      AND profile_id = ?2
                      AND (
                          updated_at_unix_ms > ?3 OR
                          (updated_at_unix_ms = ?3 AND id > ?4)
                      )
                    ORDER BY updated_at_unix_ms ASC, id ASC
                    ",
                )
                .map_err(map_sqlite_error)?;

            let rows = statement
                .query_map(
                    params![
                        normalized_dialog_key.as_str(),
                        profile_id.0.as_str(),
                        after_updated_at_unix_ms,
                        after_message_id,
                    ],
                    |row| row.get::<_, String>(0),
                )
                .map_err(map_sqlite_error)?;

            for row in rows {
                changed_message_ids.insert(row.map_err(map_sqlite_error)?);
            }
        }

        if let Some(after_updated_at_unix_ms) = after_attachment_updated_at_unix_ms {
            let after_message_id = after_attachment_message_id.unwrap_or_default();
            let mut statement = self
                .connection
                .prepare(
                    "
                    SELECT message_id
                    FROM message_attachments
                    WHERE dialog_key = ?1
                      AND profile_id = ?2
                      AND (
                          updated_at_unix_ms > ?3 OR
                          (updated_at_unix_ms = ?3 AND message_id > ?4)
                      )
                    ORDER BY updated_at_unix_ms ASC, message_id ASC
                    ",
                )
                .map_err(map_sqlite_error)?;

            let rows = statement
                .query_map(
                    params![
                        normalized_dialog_key.as_str(),
                        profile_id.0.as_str(),
                        after_updated_at_unix_ms,
                        after_message_id,
                    ],
                    |row| row.get::<_, String>(0),
                )
                .map_err(map_sqlite_error)?;

            for row in rows {
                changed_message_ids.insert(row.map_err(map_sqlite_error)?);
            }
        }

        if let Some(after_updated_at_unix_ms) = after_message_updated_at_unix_ms {
            let after_message_id = after_message_id.unwrap_or_default();
            let mut statement = self
                .connection
                .prepare(
                    "
                    SELECT call_id
                    FROM call_sessions
                    WHERE dialog_key = ?1
                      AND profile_id = ?2
                      AND state = 'ended'
                      AND outcome IS NOT NULL
                      AND (
                          updated_at_unix_ms > ?3 OR
                          (updated_at_unix_ms = ?3 AND call_id > ?4)
                      )
                    ORDER BY updated_at_unix_ms ASC, call_id ASC
                    ",
                )
                .map_err(map_sqlite_error)?;

            let rows = statement
                .query_map(
                    params![
                        normalized_dialog_key.as_str(),
                        profile_id.0.as_str(),
                        after_updated_at_unix_ms,
                        after_message_id.strip_prefix("call-history:").unwrap_or(after_message_id),
                    ],
                    |row| row.get::<_, String>(0),
                )
                .map_err(map_sqlite_error)?;

            for row in rows {
                changed_message_ids
                    .insert(format!("call-history:{}", row.map_err(map_sqlite_error)?));
            }
        }

        if changed_message_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut messages = Vec::with_capacity(changed_message_ids.len());
        for message_id in changed_message_ids {
            if message_id.starts_with("call-history:") {
                if let Some(message) = self
                    .list_call_history_messages_for_dialog(&normalized_dialog_key)?
                    .into_iter()
                    .find(|message| message.id == message_id)
                {
                    messages.push(message);
                }
            } else {
                messages.push(self.get_message_by_id(&message_id)?);
            }
        }

        messages.sort_by(|left, right| {
            left.created_at_unix_ms
                .cmp(&right.created_at_unix_ms)
                .then_with(|| left.id.cmp(&right.id))
        });

        Ok(messages)
    }

    pub fn get_latest_message_for_dialog(
        &self,
        dialog_key: &str,
    ) -> OmniResult<Option<ChatMessageRecord>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let trimmed = dialog_key.trim();
        let message = self
            .connection
            .query_row(
                "
                SELECT
                    id,
                    dialog_key,
                    direction,
                    body,
                    status,
                    remote_delivered_at_unix_ms,
                    created_at_unix_ms,
                    updated_at_unix_ms
                FROM messages
                WHERE profile_id = ?2 AND dialog_key = ?1
                ORDER BY created_at_unix_ms DESC, id DESC
                LIMIT 1
                ",
                params![trimmed, profile_id.0],
                map_message_row,
            )
            .optional()
            .map_err(map_sqlite_error)?;

        let latest_message = message
            .map(|value| self.attach_runtime_info_to_message(value))
            .transpose()?;
        let latest_call_message = if trimmed.starts_with(omni_types::DIALOG_KEY_INSTANCE_PREFIX)
            || trimmed.starts_with(omni_types::DIALOG_KEY_ACCOUNT_PREFIX)
        {
            self.get_latest_call_history_message_for_dialog(trimmed)?
        } else {
            None
        };

        Ok(select_latest_message(latest_message, latest_call_message))
    }

    pub fn list_latest_messages_by_dialog(
        &self,
    ) -> OmniResult<std::collections::HashMap<String, ChatMessageRecord>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT
                    id,
                    dialog_key,
                    direction,
                    body,
                    status,
                    remote_delivered_at_unix_ms,
                    created_at_unix_ms,
                    updated_at_unix_ms
                FROM messages
                WHERE id IN (
                    -- One step per dialog down the (profile, dialog, created, id) index: the next
                    -- dialog key, then that dialog's newest message. Reading every message of the
                    -- profile to rank it (a window function) grew with the whole history.
                    WITH RECURSIVE dialogs(dialog_key) AS (
                        SELECT (SELECT MIN(dialog_key) FROM messages WHERE profile_id = ?1 AND dialog_key > '')
                        UNION ALL
                        SELECT (
                            SELECT MIN(dialog_key) FROM messages
                            WHERE profile_id = ?1 AND dialog_key > dialogs.dialog_key
                        )
                        FROM dialogs
                        WHERE dialogs.dialog_key IS NOT NULL
                    )
                    SELECT (
                        SELECT id FROM messages
                        WHERE profile_id = ?1 AND dialog_key = dialogs.dialog_key
                        ORDER BY created_at_unix_ms DESC, id DESC
                        LIMIT 1
                    )
                    FROM dialogs
                    WHERE dialogs.dialog_key IS NOT NULL
                )
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0], map_message_row)
            .map_err(map_sqlite_error)?;
        let mut messages_by_dialog = std::collections::HashMap::new();
        for row in rows {
            let message = self.attach_runtime_info_to_message(row.map_err(map_sqlite_error)?)?;
            messages_by_dialog.insert(message.dialog_key.clone(), message);
        }

        for (dialog_key, call_message) in self.list_latest_call_history_messages_by_dialog()? {
            match messages_by_dialog.remove(&dialog_key) {
                Some(existing) => {
                    messages_by_dialog.insert(
                        dialog_key,
                        select_latest_message(Some(existing), Some(call_message)).expect("latest"),
                    );
                }
                None => {
                    messages_by_dialog.insert(dialog_key, call_message);
                }
            }
        }

        Ok(messages_by_dialog)
    }

    pub fn get_dialog_message_refresh_summary(
        &self,
        dialog_key: &str,
    ) -> OmniResult<DialogMessageRefreshSummary> {
        let profile_id = self.resolve_transition_profile_id()?;
        let normalized_dialog_key = normalize_dialog_key(dialog_key);
        let message_count = self
            .connection
            .query_row(
                "
                SELECT COUNT(*)
                FROM messages
                WHERE dialog_key = ?1
                  AND profile_id = ?2
                ",
                params![normalized_dialog_key, profile_id.0],
                |row| row.get::<_, i64>(0),
            )
            .map_err(map_sqlite_error)? as usize;
        let call_message_count = self
            .connection
            .query_row(
                "
                SELECT COUNT(*)
                FROM call_sessions
                WHERE dialog_key = ?1
                  AND profile_id = ?2
                  AND state = 'ended'
                  AND outcome IS NOT NULL
                ",
                params![normalized_dialog_key, profile_id.0],
                |row| row.get::<_, i64>(0),
            )
            .map_err(map_sqlite_error)? as usize;
        let latest_message = self
            .connection
            .query_row(
                "
                SELECT id, updated_at_unix_ms
                FROM messages
                WHERE dialog_key = ?1
                  AND profile_id = ?2
                ORDER BY updated_at_unix_ms DESC, id DESC
                LIMIT 1
                ",
                params![normalized_dialog_key, profile_id.0],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let latest_call_message = self
            .connection
            .query_row(
                "
                SELECT call_id, updated_at_unix_ms
                FROM call_sessions
                WHERE dialog_key = ?1
                  AND profile_id = ?2
                  AND state = 'ended'
                  AND outcome IS NOT NULL
                ORDER BY updated_at_unix_ms DESC, call_id DESC
                LIMIT 1
                ",
                params![normalized_dialog_key, profile_id.0],
                |row| Ok((format!("call-history:{}", row.get::<_, String>(0)?), row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let last_security_projection_updated_at_unix_ms = self
            .connection
            .query_row(
                "
                SELECT updated_at_unix_ms
                FROM message_security_metadata
                WHERE dialog_key = ?1
                  AND profile_id = ?2
                ORDER BY updated_at_unix_ms DESC, message_id DESC
                LIMIT 1
                ",
                params![normalized_dialog_key, profile_id.0],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let last_security_projection_message_id = self
            .connection
            .query_row(
                "
                SELECT message_id
                FROM message_security_metadata
                WHERE dialog_key = ?1
                  AND profile_id = ?2
                ORDER BY updated_at_unix_ms DESC, message_id DESC
                LIMIT 1
                ",
                params![normalized_dialog_key, profile_id.0],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let last_attachment_updated_at_unix_ms = self
            .connection
            .query_row(
                "
                SELECT updated_at_unix_ms
                FROM message_attachments
                WHERE dialog_key = ?1
                  AND profile_id = ?2
                ORDER BY updated_at_unix_ms DESC, message_id DESC
                LIMIT 1
                ",
                params![normalized_dialog_key, profile_id.0],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let last_attachment_message_id = self
            .connection
            .query_row(
                "
                SELECT message_id
                FROM message_attachments
                WHERE dialog_key = ?1
                  AND profile_id = ?2
                ORDER BY updated_at_unix_ms DESC, message_id DESC
                LIMIT 1
                ",
                params![normalized_dialog_key, profile_id.0],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;

        Ok(DialogMessageRefreshSummary {
            dialog_key: normalized_dialog_key,
            message_count: message_count + call_message_count,
            latest_message_id: select_latest_refresh_marker(
                latest_message.as_ref(),
                latest_call_message.as_ref(),
            )
            .map(|(message_id, _)| message_id.to_string()),
            last_message_updated_at_unix_ms: select_latest_refresh_marker(
                latest_message.as_ref(),
                latest_call_message.as_ref(),
            )
            .map(|(_, updated_at_unix_ms)| updated_at_unix_ms),
            last_attachment_updated_at_unix_ms,
            last_attachment_message_id,
            last_security_projection_updated_at_unix_ms,
            last_security_projection_message_id,
        })
    }

    pub fn get_chat_thread_list_refresh_summary(
        &self,
    ) -> OmniResult<omni_types::ChatThreadListRefreshSummary> {
        let profile_id = self.resolve_transition_profile_id()?;
        let thread_count = self
            .connection
            .query_row(
                "
                SELECT COUNT(*)
                FROM contacts c
                WHERE c.profile_id = ?1
                  AND c.removed_at_unix_ms IS NULL
                  AND c.relationship_state = 'approved'
                  AND (
                      c.remote_instance_id IS NOT NULL
                      OR EXISTS (
                          SELECT 1
                          FROM contact_dialog_links l
                          WHERE l.contact_id = c.contact_id
                            AND l.profile_id = c.profile_id
                            AND l.is_primary = 1
                      )
                  )
                ",
                params![profile_id.0],
                |row| row.get::<_, i64>(0),
            )
            .map_err(map_sqlite_error)? as usize;
        let last_contact_updated_at_unix_ms = self
            .connection
            .query_row(
                "
                SELECT MAX(updated_at_unix_ms)
                FROM contacts
                WHERE profile_id = ?1
                  AND removed_at_unix_ms IS NULL
                  AND relationship_state = 'approved'
                ",
                params![profile_id.0],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(map_sqlite_error)?;
        let last_dialog_link_updated_at_unix_ms = self
            .connection
            .query_row(
                "
                SELECT MAX(updated_at_unix_ms)
                FROM contact_dialog_links
                WHERE profile_id = ?1
                  AND is_primary = 1
                ",
                params![profile_id.0],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(map_sqlite_error)?;
        let last_public_profile_updated_at_unix_ms = self
            .connection
            .query_row(
                "
                SELECT MAX(public_profile_updated_at_unix_ms)
                FROM contacts
                WHERE profile_id = ?1
                  AND removed_at_unix_ms IS NULL
                  AND relationship_state = 'approved'
                ",
                params![profile_id.0],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(map_sqlite_error)?;
        let last_message_updated_at_unix_ms = self
            .connection
            .query_row(
                "
                SELECT MAX(updated_at_unix_ms)
                FROM messages
                WHERE profile_id = ?1
                  AND dialog_key <> ''
                ",
                params![profile_id.0],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(map_sqlite_error)?;
        let last_call_updated_at_unix_ms = self
            .connection
            .query_row(
                "
                SELECT MAX(updated_at_unix_ms)
                FROM call_sessions
                WHERE profile_id = ?1
                  AND dialog_key <> ''
                  AND state = 'ended'
                  AND outcome IS NOT NULL
                ",
                params![profile_id.0],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(map_sqlite_error)?;
        let last_attachment_updated_at_unix_ms = self
            .connection
            .query_row(
                "
                SELECT MAX(updated_at_unix_ms)
                FROM message_attachments
                WHERE profile_id = ?1
                  AND dialog_key <> ''
                ",
                params![profile_id.0],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(map_sqlite_error)?;

        Ok(omni_types::ChatThreadListRefreshSummary {
            thread_count,
            last_contact_updated_at_unix_ms,
            last_dialog_link_updated_at_unix_ms,
            last_public_profile_updated_at_unix_ms,
            last_message_updated_at_unix_ms: max_option_i64(
                last_message_updated_at_unix_ms,
                last_call_updated_at_unix_ms,
            ),
            last_attachment_updated_at_unix_ms,
            last_read_mark_updated_at_unix_ms: self.last_read_mark_updated_at_unix_ms()?,
        })
    }

    pub fn delete_dialog_history(&self, dialog_key: &str) -> OmniResult<()> {
        let profile_id = self.resolve_transition_profile_id()?;
        let normalized_dialog_key = normalize_dialog_key(dialog_key);
        self.delete_attachment_assets_for_dialog(&normalized_dialog_key)?;

        self.connection
            .execute(
                "DELETE FROM messages WHERE dialog_key = ?1 AND profile_id = ?2",
                params![normalized_dialog_key, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "DELETE FROM outbound_queue WHERE dialog_key = ?1 AND profile_id = ?2",
                params![normalized_dialog_key, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "DELETE FROM inbound_dedup WHERE dialog_key = ?1 AND profile_id = ?2",
                params![normalized_dialog_key, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "DELETE FROM delivery_receipts WHERE dialog_key = ?1 AND profile_id = ?2",
                params![normalized_dialog_key, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "DELETE FROM message_security_metadata WHERE dialog_key = ?1 AND profile_id = ?2",
                params![normalized_dialog_key, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "DELETE FROM call_sessions WHERE dialog_key = ?1 AND profile_id = ?2",
                params![normalized_dialog_key, profile_id.0],
            )
            .map_err(map_sqlite_error)?;

        Ok(())
    }

    pub fn update_message_status(
        &self,
        id: &str,
        status: MessageStatus,
        updated_at_unix_ms: i64,
    ) -> OmniResult<ChatMessageRecord> {
        let profile_id = self.resolve_transition_profile_id()?;
        let remote_delivered_at_unix_ms =
            matches!(status, MessageStatus::Delivered).then_some(updated_at_unix_ms);
        let updated_rows = self
            .connection
            .execute(
                "
                UPDATE messages
                SET status = ?2,
                    updated_at_unix_ms = ?3,
                    remote_delivered_at_unix_ms =
                        CASE
                            WHEN ?4 IS NULL THEN remote_delivered_at_unix_ms
                            ELSE ?4
                        END
                WHERE id = ?1
                  AND profile_id = ?5
                ",
                params![
                    id,
                    status_to_db(status),
                    updated_at_unix_ms,
                    remote_delivered_at_unix_ms,
                    profile_id.0
                ],
            )
            .map_err(map_sqlite_error)?;

        if updated_rows == 0 {
            return Err(OmniError::Storage(format!(
                "message with id '{id}' was not found"
            )));
        }

        let message = self.get_message_by_id(id)?;
        if message.direction == ChatDirection::Outgoing {
            let queue_metadata = self.queue_metadata_for_message(id)?;
            self.upsert_outbound_queue(
                &message.id,
                &message.dialog_key,
                message.status,
                queue_metadata.attempt_count,
                queue_metadata.last_attempt_unix_ms,
                None,
                queue_metadata.last_failure_kind,
                queue_metadata.last_failure_message.as_deref(),
                message.created_at_unix_ms,
                updated_at_unix_ms,
            )?;

            if let Some(remote_delivered_at_unix_ms) = message.remote_delivered_at_unix_ms {
                self.connection
                    .execute(
                        "
                        INSERT INTO delivery_receipts (
                            message_id,
                            dialog_key,
                            delivered_at_unix_ms,
                            recorded_at_unix_ms,
                            profile_id
                        )
                        VALUES (?1, ?2, ?3, ?4, ?5)
                        ON CONFLICT(message_id) DO UPDATE SET
                            delivered_at_unix_ms = excluded.delivered_at_unix_ms,
                            recorded_at_unix_ms = excluded.recorded_at_unix_ms
                        ",
                        params![
                            message.id,
                            message.dialog_key,
                            remote_delivered_at_unix_ms,
                            updated_at_unix_ms,
                            profile_id.0
                        ],
                    )
                    .map_err(map_sqlite_error)?;
            }
        }

        Ok(message)
    }

    pub fn get_message(&self, id: &str) -> OmniResult<ChatMessageRecord> {
        if let Some(call_id) = id.strip_prefix("call-history:") {
            return self
                .get_call_summary(call_id)?
                .and_then(|summary| build_call_history_message_record(&summary))
                .ok_or_else(|| OmniError::Storage(format!("message with id '{id}' was not found")));
        }
        self.get_message_by_id(id)
    }

    pub fn outbound_queue_snapshot(&self) -> OmniResult<OutboundQueueSnapshot> {
        let profile_id = self.resolve_transition_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT
                    message_id,
                    dialog_key,
                    status,
                    attempt_count,
                    next_attempt_unix_ms,
                    last_attempt_unix_ms,
                    last_failure_kind,
                    last_failure_message,
                    remote_delivered_at_unix_ms,
                    created_at_unix_ms,
                    updated_at_unix_ms
                FROM outbound_queue
                WHERE profile_id = ?1
                ORDER BY created_at_unix_ms ASC, message_id ASC
                ",
            )
            .map_err(map_sqlite_error)?;

        let rows = statement
            .query_map(params![profile_id.0], map_outbound_queue_row)
            .map_err(map_sqlite_error)?;

        let mut items = Vec::new();
        for row in rows {
            items.push(row.map_err(map_sqlite_error)?);
        }

        Ok(OutboundQueueSnapshot {
            queued_count: items
                .iter()
                .filter(|item| item.status == MessageStatus::Queued)
                .count(),
            sending_count: items
                .iter()
                .filter(|item| item.status == MessageStatus::Sending)
                .count(),
            retrying_count: items
                .iter()
                .filter(|item| item.status == MessageStatus::Retrying)
                .count(),
            sent_waiting_ack_count: items
                .iter()
                .filter(|item| item.status == MessageStatus::Sent)
                .count(),
            failed_count: items
                .iter()
                .filter(|item| item.status == MessageStatus::Failed)
                .count(),
            items,
        })
    }

    pub fn queue_diagnostics(
        &self,
        last_delivery_error: Option<String>,
    ) -> OmniResult<QueueDiagnosticsSnapshot> {
        let snapshot = self.outbound_queue_snapshot()?;
        let mut by_dialog =
            std::collections::BTreeMap::<String, (DialogQueueDiagnostics, i64)>::new();
        let mut last_failure: Option<(QueueFailureKind, String, i64)> = None;

        for item in &snapshot.items {
            let (entry, last_failure_timestamp) =
                by_dialog.entry(item.dialog_key.clone()).or_insert((
                    DialogQueueDiagnostics {
                        dialog_key: item.dialog_key.clone(),
                        pending_count: 0,
                        sent_waiting_ack_count: 0,
                        retrying_count: 0,
                        failed_count: 0,
                        last_failure_kind: None,
                        last_failure_message: None,
                    },
                    i64::MIN,
                ));

            entry.pending_count += 1;
            match item.status {
                MessageStatus::Sent => entry.sent_waiting_ack_count += 1,
                MessageStatus::Retrying => entry.retrying_count += 1,
                MessageStatus::Failed => entry.failed_count += 1,
                _ => {}
            }

            if let Some(failure_kind) = item.last_failure_kind {
                let failure_message = item
                    .last_failure_message
                    .clone()
                    .unwrap_or_else(|| failure_kind_to_default_message(failure_kind).to_string());
                if item.updated_at_unix_ms >= *last_failure_timestamp {
                    entry.last_failure_kind = Some(failure_kind);
                    entry.last_failure_message = Some(failure_message.clone());
                    *last_failure_timestamp = item.updated_at_unix_ms;
                }

                if last_failure
                    .as_ref()
                    .map(|(_, _, timestamp)| item.updated_at_unix_ms >= *timestamp)
                    .unwrap_or(true)
                {
                    last_failure = Some((failure_kind, failure_message, item.updated_at_unix_ms));
                }
            }
        }

        Ok(QueueDiagnosticsSnapshot {
            queue_length: snapshot.items.len(),
            queued_count: snapshot.queued_count,
            sending_count: snapshot.sending_count,
            retrying_count: snapshot.retrying_count,
            sent_waiting_ack_count: snapshot.sent_waiting_ack_count,
            failed_count: snapshot.failed_count,
            last_delivery_error,
            last_failure_kind: last_failure.as_ref().map(|(kind, _, _)| *kind),
            last_failure_message: last_failure.map(|(_, message, _)| message),
            pending_by_dialog: by_dialog
                .into_values()
                .map(|(diagnostics, _)| diagnostics)
                .collect(),
        })
    }

    pub fn rebind_pending_dialog_scope(
        &self,
        legacy_dialog_key: &str,
        stable_dialog_key: &str,
        updated_at_unix_ms: i64,
    ) -> OmniResult<()> {
        let profile_id = self.resolve_transition_profile_id()?;
        let stable_dialog_key = normalize_dialog_key(stable_dialog_key);
        self.connection
            .execute(
                "
                UPDATE messages
                SET dialog_key = ?2,
                    updated_at_unix_ms = ?3
                WHERE dialog_key = ?1
                  AND direction = 'outgoing'
                  AND status IN ('queued', 'sending', 'sent', 'retrying', 'failed')
                  AND profile_id = ?4
                ",
                params![legacy_dialog_key, stable_dialog_key, updated_at_unix_ms, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                UPDATE outbound_queue
                SET dialog_key = ?2,
                    updated_at_unix_ms = ?3
                WHERE dialog_key = ?1
                  AND profile_id = ?4
                ",
                params![legacy_dialog_key, stable_dialog_key, updated_at_unix_ms, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                UPDATE inbound_dedup
                SET dialog_key = ?2
                WHERE dialog_key = ?1
                  AND profile_id = ?3
                ",
                params![legacy_dialog_key, stable_dialog_key, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                UPDATE call_sessions
                SET dialog_key = ?2,
                    updated_at_unix_ms = ?3
                WHERE dialog_key = ?1
                  AND profile_id = ?4
                ",
                params![legacy_dialog_key, stable_dialog_key, updated_at_unix_ms, profile_id.0],
            )
            .map_err(map_sqlite_error)?;

        Ok(())
    }

    pub fn rebind_dialog_scope(
        &self,
        legacy_dialog_key: &str,
        stable_dialog_key: &str,
        updated_at_unix_ms: i64,
    ) -> OmniResult<()> {
        let profile_id = self.resolve_transition_profile_id()?;
        let legacy_dialog_key = normalize_dialog_key(legacy_dialog_key);
        let stable_dialog_key = normalize_dialog_key(stable_dialog_key);
        if legacy_dialog_key == stable_dialog_key {
            return Ok(());
        }

        self.connection
            .execute(
                "
                UPDATE messages
                SET dialog_key = ?2,
                    updated_at_unix_ms = ?3
                WHERE dialog_key = ?1
                  AND profile_id = ?4
                ",
                params![legacy_dialog_key, stable_dialog_key, updated_at_unix_ms, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                UPDATE outbound_queue
                SET dialog_key = ?2,
                    updated_at_unix_ms = ?3
                WHERE dialog_key = ?1
                  AND profile_id = ?4
                ",
                params![legacy_dialog_key, stable_dialog_key, updated_at_unix_ms, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                UPDATE inbound_dedup
                SET dialog_key = ?2
                WHERE dialog_key = ?1
                  AND profile_id = ?3
                ",
                params![legacy_dialog_key, stable_dialog_key, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                UPDATE delivery_receipts
                SET dialog_key = ?2,
                    recorded_at_unix_ms = ?3
                WHERE dialog_key = ?1
                  AND profile_id = ?4
                ",
                params![legacy_dialog_key, stable_dialog_key, updated_at_unix_ms, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                UPDATE message_attachments
                SET dialog_key = ?2,
                    updated_at_unix_ms = ?3
                WHERE dialog_key = ?1
                  AND profile_id = ?4
                ",
                params![legacy_dialog_key, stable_dialog_key, updated_at_unix_ms, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                UPDATE message_security_metadata
                SET dialog_key = ?2,
                    updated_at_unix_ms = ?3
                WHERE dialog_key = ?1
                  AND profile_id = ?4
                ",
                params![legacy_dialog_key, stable_dialog_key, updated_at_unix_ms, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                UPDATE call_sessions
                SET dialog_key = ?2,
                    updated_at_unix_ms = ?3
                WHERE dialog_key = ?1
                  AND profile_id = ?4
                ",
                params![legacy_dialog_key, stable_dialog_key, updated_at_unix_ms, profile_id.0],
            )
            .map_err(map_sqlite_error)?;

        Ok(())
    }
}

fn select_latest_message(
    left: Option<ChatMessageRecord>,
    right: Option<ChatMessageRecord>,
) -> Option<ChatMessageRecord> {
    match (left, right) {
        (Some(left), Some(right)) => {
            if left.created_at_unix_ms > right.created_at_unix_ms
                || (left.created_at_unix_ms == right.created_at_unix_ms && left.id >= right.id)
            {
                Some(left)
            } else {
                Some(right)
            }
        }
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn select_latest_refresh_marker<'a>(
    left: Option<&'a (String, i64)>,
    right: Option<&'a (String, i64)>,
) -> Option<(&'a str, i64)> {
    match (left, right) {
        (Some(left), Some(right)) => {
            if left.1 > right.1 || (left.1 == right.1 && left.0 >= right.0) {
                Some((left.0.as_str(), left.1))
            } else {
                Some((right.0.as_str(), right.1))
            }
        }
        (Some(left), None) => Some((left.0.as_str(), left.1)),
        (None, Some(right)) => Some((right.0.as_str(), right.1)),
        (None, None) => None,
    }
}

fn max_option_i64(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}
