// The account's OTHER devices as message peers (docs/decisions/multi-device-equal-nodes.md, 2.4):
// where each one answers, and the copies of sent messages they hand each other.

impl Storage {
    /// A device of our own account showed up on `dialog_key` with a valid certificate chain:
    /// remember it (its certificate, and where it answers).
    pub fn record_own_device(
        &self,
        chain: &omni_crypto::DeviceCertificateChainV1,
        device_label: &str,
        dialog_key: &str,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        // A device certified by an older working key than the account's current one is out: the
        // account moved on without it.
        if self
            .current_working_key_version(&chain.device.account_id)?
            .is_some_and(|current| chain.working.version < current)
        {
            return Err(OmniError::InvalidInput(
                "the device is certified by a superseded working key".to_string(),
            ));
        }
        self.store_account_device(chain, device_label, now_unix_ms)?;
        self.mark_key_move_delivered(chain)?;
        self.connection
            .execute(
                "UPDATE account_devices SET dialog_key = ?3 WHERE account_id = ?1 AND device_id = ?2",
                params![
                    chain.device.account_id.0.as_str(),
                    chain.device.device_id.0.as_str(),
                    omni_types::normalize_dialog_key(dialog_key),
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// We heard this device of our account at `at_unix_ms` (a link, its fresh beacon). Never moves back.
    pub fn note_own_device_heard(&self, account_id: &AccountId, device_id: &DeviceId, at_unix_ms: i64) -> OmniResult<()> {
        self.connection
            .execute(
                "UPDATE account_devices SET last_heard_at_unix_ms = MAX(COALESCE(last_heard_at_unix_ms, 0), ?3) \
                 WHERE account_id = ?1 AND device_id = ?2",
                params![account_id.0.as_str(), device_id.0.as_str(), at_unix_ms],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// When each device of `account_id` was last heard; one never heard counts from when we first
    /// learned of it (it was alive then — and a copy that went silent before this column existed is
    /// not taken for a fresh one).
    pub fn own_devices_last_heard(&self, account_id: &AccountId) -> OmniResult<std::collections::HashMap<DeviceId, i64>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT device_id, COALESCE(last_heard_at_unix_ms, first_seen_at_unix_ms) FROM account_devices \
                 WHERE account_id = ?1",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![account_id.0.as_str()], |row| {
                Ok((DeviceId(row.get::<_, String>(0)?), row.get::<_, i64>(1)?))
            })
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<_, _>>().map_err(map_sqlite_error)
    }

    /// Our other devices we can reach: not this one, not revoked, seen on a dialog.
    pub fn list_own_device_dialogs(&self) -> OmniResult<Vec<(DeviceId, String)>> {
        let (Some(account), Some(device)) = (self.get_local_account()?, self.get_local_device()?)
        else {
            return Ok(Vec::new());
        };
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT device_id, dialog_key
                FROM account_devices
                WHERE account_id = ?1
                  AND device_id <> ?2
                  AND revoked_at_unix_ms IS NULL
                  AND dialog_key IS NOT NULL
                ORDER BY first_seen_at_unix_ms ASC, device_id ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(
                params![account.descriptor.account_id.0.as_str(), device.device_id.0.as_str()],
                |row| Ok((DeviceId(row.get(0)?), row.get::<_, String>(1)?)),
            )
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    /// Is `(account_id, device_id)` another device of our own account that we know of?
    pub fn is_own_other_device(&self, account_id: &AccountId, device_id: &DeviceId) -> OmniResult<bool> {
        let (Some(account), Some(device)) = (self.get_local_account()?, self.get_local_device()?)
        else {
            return Ok(false);
        };
        if &account.descriptor.account_id != account_id || &device.device_id == device_id {
            return Ok(false);
        }
        let known: Option<i64> = self
            .connection
            .query_row(
                "
                SELECT 1 FROM account_devices
                WHERE account_id = ?1 AND device_id = ?2 AND revoked_at_unix_ms IS NULL
                ",
                params![account_id.0.as_str(), device_id.0.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        Ok(known.is_some())
    }

    /// Our own message, sent from another of our devices: shown in the chat as sent, never queued
    /// here (the device that wrote it delivers it) — or as delivered, when the contact's receipt
    /// got here before the copy did. `false` when we already have it.
    pub fn insert_sent_copy(
        &self,
        id: &str,
        chat_dialog_key: &str,
        body: &str,
        created_at_unix_ms: i64,
        from_device_id: &DeviceId,
    ) -> OmniResult<bool> {
        if self.get_message_by_id(id).is_ok() {
            return Ok(false);
        }
        self.insert_outgoing_row(
            id,
            chat_dialog_key,
            body,
            MessageStatus::Sent,
            created_at_unix_ms,
        )?;
        self.connection
            .execute(
                "UPDATE messages SET copied_from_device_id = ?2 WHERE id = ?1",
                params![id, from_device_id.0.as_str()],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                UPDATE messages
                SET status = 'delivered',
                    remote_delivered_at_unix_ms = (
                        SELECT delivered_at_unix_ms FROM delivery_receipts WHERE message_id = ?1
                    )
                WHERE id = ?1
                  AND EXISTS (SELECT 1 FROM delivery_receipts WHERE message_id = ?1)
                ",
                params![id],
            )
            .map_err(map_sqlite_error)?;
        Ok(true)
    }

    /// Outgoing chat messages of every chat since `since_unix_ms` that THIS device wrote (not copies
    /// from our other devices) and that already left the queue — what our other devices may lack.
    pub fn list_own_sent_messages_since(&self, since_unix_ms: i64) -> OmniResult<Vec<ChatMessageRecord>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT m.id, m.dialog_key, m.direction, m.body, m.status,
                       m.remote_delivered_at_unix_ms, m.created_at_unix_ms, m.updated_at_unix_ms
                FROM messages m
                WHERE m.profile_id = ?1
                  AND m.direction = 'outgoing'
                  AND m.copied_from_device_id IS NULL
                  AND m.dialog_key LIKE 'dialog:%'
                  AND m.status IN ('sent', 'delivered')
                  AND m.created_at_unix_ms >= ?2
                ORDER BY m.created_at_unix_ms ASC, m.id ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0.as_str(), since_unix_ms], map_message_row)
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }
}
