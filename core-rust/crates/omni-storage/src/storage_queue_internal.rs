impl Storage {
    pub fn pending_queue_for_dialog(
        &self,
        dialog_key: &str,
        now_unix_ms: i64,
    ) -> OmniResult<Vec<OutboundQueueRecord>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let dialog_key = normalize_dialog_key(dialog_key);
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
                WHERE dialog_key = ?1
                  AND profile_id = ?2
                  AND status IN ('queued', 'retrying')
                ORDER BY created_at_unix_ms ASC, message_id ASC
                ",
            )
            .map_err(map_sqlite_error)?;

        let rows = statement
            .query_map(params![dialog_key, profile_id.0], map_outbound_queue_row)
            .map_err(map_sqlite_error)?;

        let mut items = Vec::new();
        for row in rows {
            let item = row.map_err(map_sqlite_error)?;
            if item
                .next_attempt_unix_ms
                .map(|next| next <= now_unix_ms)
                .unwrap_or(true)
            {
                items.push(item);
            }
        }

        Ok(items)
    }

    /// Dialogs that hold at least one queued/retrying message due for another attempt.
    /// The queue tick walks these so a chat that is not the active transport dialog is not left
    /// waiting for the moment it becomes active.
    pub fn dialogs_with_due_pending_queue(&self, now_unix_ms: i64) -> OmniResult<Vec<String>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT DISTINCT dialog_key
                FROM outbound_queue
                WHERE profile_id = ?1
                  AND status IN ('queued', 'retrying')
                  AND (next_attempt_unix_ms IS NULL OR next_attempt_unix_ms <= ?2)
                ORDER BY dialog_key ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0, now_unix_ms], |row| row.get::<_, String>(0))
            .map_err(map_sqlite_error)?;
        let mut dialogs = Vec::new();
        for row in rows {
            dialogs.push(row.map_err(map_sqlite_error)?);
        }
        Ok(dialogs)
    }

    pub fn mark_queue_attempt_started(
        &self,
        message_id: &str,
        started_at_unix_ms: i64,
    ) -> OmniResult<ChatMessageRecord> {
        let existing = self.get_message_by_id(message_id)?;
        let attempt_count = self.queue_attempt_count_for_message(message_id)? + 1;
        let queue_metadata = self.queue_metadata_for_message(message_id)?;
        self.update_message_status(message_id, MessageStatus::Sending, started_at_unix_ms)?;
        self.upsert_outbound_queue(
            message_id,
            &existing.dialog_key,
            MessageStatus::Sending,
            attempt_count,
            Some(started_at_unix_ms),
            None,
            queue_metadata.last_failure_kind,
            queue_metadata.last_failure_message.as_deref(),
            existing.created_at_unix_ms,
            started_at_unix_ms,
        )?;

        self.get_message_by_id(message_id)
    }

    pub fn mark_queue_retry(
        &self,
        message_id: &str,
        reason_kind: QueueFailureKind,
        reason_message: &str,
        next_attempt_unix_ms: i64,
        updated_at_unix_ms: i64,
    ) -> OmniResult<ChatMessageRecord> {
        let existing = self.get_message_by_id(message_id)?;
        let queue_metadata = self.queue_metadata_for_message(message_id)?;
        self.update_message_status(message_id, MessageStatus::Retrying, updated_at_unix_ms)?;
        self.upsert_outbound_queue(
            message_id,
            &existing.dialog_key,
            MessageStatus::Retrying,
            queue_metadata.attempt_count,
            queue_metadata.last_attempt_unix_ms,
            Some(next_attempt_unix_ms),
            Some(reason_kind),
            Some(reason_message),
            existing.created_at_unix_ms,
            updated_at_unix_ms,
        )?;

        self.get_message_by_id(message_id)
    }

    pub fn mark_queue_failed(
        &self,
        message_id: &str,
        reason_kind: QueueFailureKind,
        reason_message: &str,
        updated_at_unix_ms: i64,
    ) -> OmniResult<ChatMessageRecord> {
        let existing = self.get_message_by_id(message_id)?;
        let queue_metadata = self.queue_metadata_for_message(message_id)?;
        self.update_message_status(message_id, MessageStatus::Failed, updated_at_unix_ms)?;
        self.upsert_outbound_queue(
            message_id,
            &existing.dialog_key,
            MessageStatus::Failed,
            queue_metadata.attempt_count,
            queue_metadata.last_attempt_unix_ms,
            None,
            Some(reason_kind),
            Some(reason_message),
            existing.created_at_unix_ms,
            updated_at_unix_ms,
        )?;

        self.get_message_by_id(message_id)
    }

    pub fn record_delivery_receipt(
        &self,
        message_id: &str,
        dialog_key: &str,
        delivered_at_unix_ms: i64,
        recorded_at_unix_ms: i64,
    ) -> OmniResult<Option<ChatMessageRecord>> {
        self.with_immediate_transaction("record_delivery_receipt", |storage| {
            let profile_id = storage.resolve_transition_profile_id()?;
            let normalized_dialog_key = normalize_dialog_key(dialog_key);
            // Kept even for a message not here yet: the contact tells every device of ours, and the
            // copy of a message sent from another of them may still be on its way
            // (`insert_sent_copy` applies the receipt when it lands).
            storage
                .connection
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
                        dialog_key = excluded.dialog_key,
                        delivered_at_unix_ms = excluded.delivered_at_unix_ms,
                        recorded_at_unix_ms = excluded.recorded_at_unix_ms,
                        profile_id = excluded.profile_id
                    ",
                    params![
                        message_id,
                        normalized_dialog_key,
                        delivered_at_unix_ms,
                        recorded_at_unix_ms,
                        profile_id.0,
                    ],
                )
                .map_err(map_sqlite_error)?;

            let updated = storage
                .connection
                .execute(
                    "
                    UPDATE messages
                    SET status = 'delivered',
                        remote_delivered_at_unix_ms = ?2,
                        updated_at_unix_ms = ?3
                    WHERE id = ?1
                      AND direction = 'outgoing'
                      AND profile_id = ?4
                    ",
                    params![message_id, delivered_at_unix_ms, recorded_at_unix_ms, profile_id.0],
                )
                .map_err(map_sqlite_error)?;

            if updated == 0 {
                return Ok(None);
            }

            let message = storage.get_message_by_id(message_id)?;
            storage
                .connection
                .execute(
                    "DELETE FROM outbound_queue WHERE message_id = ?1 AND profile_id = ?2",
                    params![message_id, profile_id.0],
                )
                .map_err(map_sqlite_error)?;

            Ok(Some(message))
        })
    }

    pub fn queue_attempt_count_for_message(&self, message_id: &str) -> OmniResult<i64> {
        let profile_id = self.resolve_transition_profile_id()?;
        self.connection
            .query_row(
                "SELECT attempt_count FROM outbound_queue WHERE message_id = ?1 AND profile_id = ?2",
                params![message_id, profile_id.0],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_error)
            .map(|value| value.unwrap_or(0))
    }

    fn insert_app_meta_if_missing(&self, key: &str, value: &str) -> OmniResult<()> {
        self.connection
            .execute(
                "INSERT OR IGNORE INTO app_meta (key, value) VALUES (?1, ?2)",
                params![key, value],
            )
            .map_err(map_sqlite_error)?;

        Ok(())
    }

    fn persist_local_account_and_device(
        &self,
        account: &LocalAccountSnapshot,
        device: &LocalDeviceSnapshot,
        representation_v2: Option<&omni_identity::AccountRepresentationV2>,
    ) -> OmniResult<()> {
        self.with_immediate_transaction("persist_local_account_and_device", |storage| {
            let profile_id = account
                .local_profile_summary
                .as_ref()
                .map(|value| value.profile_id.clone())
                .ok_or_else(|| {
                    OmniError::InvalidInput(
                        "profile summary is missing for local account".to_string(),
                    )
                })?;
            let profile_state = account
                .local_profile_summary
                .as_ref()
                .map(|value| profile_state_to_db(value.state))
                .ok_or_else(|| {
                    OmniError::InvalidInput(
                        "profile summary is missing for local account".to_string(),
                    )
                })?;
            storage
                .connection
                .execute(
                    "
                    INSERT INTO local_account (
                        singleton,
                        account_id,
                        display_name,
                        recovery_version,
                        recovery_code,
                        restored_from_recovery,
                        created_at_unix_ms,
                        updated_at_unix_ms,
                        account_representation_version,
                        profile_id,
                        profile_state
                    )
                    VALUES (
                        1,
                        ?1,
                        ?2,
                        ?3,
                        ?4,
                        ?5,
                        ?6,
                        ?7,
                        ?8,
                        ?9,
                        ?10
                    )
                    ON CONFLICT(account_id) DO UPDATE SET
                        display_name = excluded.display_name,
                        recovery_version = excluded.recovery_version,
                        recovery_code = excluded.recovery_code,
                        restored_from_recovery = excluded.restored_from_recovery,
                        created_at_unix_ms = excluded.created_at_unix_ms,
                        updated_at_unix_ms = excluded.updated_at_unix_ms,
                        account_representation_version = excluded.account_representation_version,
                        profile_id = excluded.profile_id,
                        profile_state = excluded.profile_state
                    ",
                    params![
                        account.descriptor.account_id.0.as_str(),
                        account.descriptor.display_name.as_str(),
                        account
                            .recovery_phrase_v2
                            .as_ref()
                            .map_or(0, |phrase| i64::from(phrase.version)),
                        "v2-managed-in-encrypted-secrets",
                        bool_to_sqlite_integer(account.restored_from_recovery),
                        account.created_at_unix_ms,
                        account.updated_at_unix_ms,
                        i64::from(account.account_representation_version),
                        profile_id.0.as_str(),
                        profile_state,
                    ],
                )
                .map_err(map_sqlite_error)?;
            storage
                .connection
                .execute(
                    "
                    INSERT INTO local_devices (
                        device_id,
                        account_id,
                        device_label,
                        is_current,
                        restored_from_recovery,
                        created_at_unix_ms,
                        updated_at_unix_ms
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                    ",
                    params![
                        device.device_id.0,
                        device.account_id.0,
                        device.device_label,
                        bool_to_sqlite_integer(device.is_current),
                        bool_to_sqlite_integer(device.restored_from_recovery),
                        device.created_at_unix_ms,
                        device.updated_at_unix_ms,
                    ],
                )
                .map_err(map_sqlite_error)?;
            storage.upsert_profile_registry_from_account(account, account.created_at_unix_ms)?;
            storage.upsert_profile_secrets_from_account(
                account,
                representation_v2,
                account.created_at_unix_ms,
            )?;
            if let Some(representation_v2) = representation_v2 {
                storage.persist_account_keys_from_root(
                    &account.descriptor.account_id,
                    &representation_v2.local_secret_state.root_secret,
                    account.created_at_unix_ms,
                )?;
            }
            storage.set_profile_current_device_id(
                &profile_id,
                &device.device_id,
                device.created_at_unix_ms,
            )?;
            storage.set_active_profile_id(Some(&profile_id))?;

            Ok(())
        })
    }

    fn persist_local_crypto_hierarchy(&self, hierarchy: &omni_crypto::LocalCryptoHierarchy) -> OmniResult<()> {
        self.with_immediate_transaction("persist_local_crypto_hierarchy", |storage| {
            storage
                .connection
                .execute(
                    "
                    INSERT INTO identity_keys (
                        account_id,
                        public_key,
                        signing_public_key,
                        private_key_protection_scheme,
                        private_key_blob,
                        signing_private_key_protection_scheme,
                        signing_private_key_blob,
                        suite_version,
                        created_at_unix_ms,
                        updated_at_unix_ms
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                    ",
                    params![
                        hierarchy.identity_key.account_id.0,
                        identity_public_key_to_wire(hierarchy.identity_key.public_key),
                        identity_signing_public_key_to_wire(
                            hierarchy.identity_key.signing_public_key,
                        ),
                        hierarchy.identity_key.protected_private_key.scheme.as_db_value(),
                        hierarchy.identity_key.protected_private_key.blob_hex,
                        hierarchy
                            .identity_key
                            .protected_signing_private_key
                            .scheme
                            .as_db_value(),
                        hierarchy.identity_key.protected_signing_private_key.blob_hex,
                        hierarchy.identity_key.suite_version as i64,
                        hierarchy.identity_key.created_at_unix_ms,
                        hierarchy.identity_key.updated_at_unix_ms,
                    ],
                )
                .map_err(map_sqlite_error)?;

            storage
                .connection
                .execute(
                    "
                    INSERT INTO device_keys (
                        device_id,
                        account_id,
                        public_key,
                        private_key_protection_scheme,
                        private_key_blob,
                        suite_version,
                        created_at_unix_ms,
                        updated_at_unix_ms
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                    ",
                    params![
                        hierarchy.device_key.device_id.0,
                        hierarchy.device_key.account_id.0,
                        device_public_key_to_wire(hierarchy.device_key.public_key),
                        hierarchy.device_key.protected_private_key.scheme.as_db_value(),
                        hierarchy.device_key.protected_private_key.blob_hex,
                        hierarchy.device_key.suite_version as i64,
                        hierarchy.device_key.created_at_unix_ms,
                        hierarchy.device_key.updated_at_unix_ms,
                    ],
                )
                .map_err(map_sqlite_error)?;

            let (signature_r, signature_s) =
                signature_to_wire(hierarchy.signed_prekey.signature.clone());
            storage
                .connection
                .execute(
                    "
                    INSERT INTO signed_prekeys (
                        prekey_id,
                        device_id,
                        public_key,
                        private_key_protection_scheme,
                        private_key_blob,
                        signature_r,
                        signature_s,
                        suite_version,
                        state,
                        created_at_unix_ms,
                        expires_at_unix_ms,
                        updated_at_unix_ms
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'active', ?9, ?10, ?11)
                    ",
                    params![
                        hierarchy.signed_prekey.prekey_id as i64,
                        hierarchy.signed_prekey.device_id.0,
                        prekey_public_key_to_wire(hierarchy.signed_prekey.public_key),
                        hierarchy.signed_prekey.protected_private_key.scheme.as_db_value(),
                        hierarchy.signed_prekey.protected_private_key.blob_hex,
                        signature_r,
                        signature_s,
                        hierarchy.signed_prekey.suite_version as i64,
                        hierarchy.signed_prekey.created_at_unix_ms,
                        hierarchy.signed_prekey.expires_at_unix_ms,
                        hierarchy.signed_prekey.updated_at_unix_ms,
                    ],
                )
                .map_err(map_sqlite_error)?;

            for prekey in &hierarchy.one_time_prekeys {
                storage
                    .connection
                    .execute(
                        "
                        INSERT INTO one_time_prekeys (
                            prekey_id,
                            device_id,
                            public_key,
                            private_key_protection_scheme,
                            private_key_blob,
                            suite_version,
                            state,
                            created_at_unix_ms,
                            consumed_at_unix_ms,
                            updated_at_unix_ms
                        )
                        VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'available', ?7, NULL, ?8)
                        ",
                        params![
                            prekey.prekey_id as i64,
                            prekey.device_id.0,
                            prekey_public_key_to_wire(prekey.public_key),
                            prekey.protected_private_key.scheme.as_db_value(),
                            prekey.protected_private_key.blob_hex,
                            prekey.suite_version as i64,
                            prekey.created_at_unix_ms,
                            prekey.updated_at_unix_ms,
                        ],
                    )
                    .map_err(map_sqlite_error)?;
            }

            Ok(())
        })
    }

    /// Долить одноразовые предключи, не трогая идентичность и подписанный предключ.
    /// См. `omni_crypto::generate_one_time_prekeys` — там разобрано, почему пул надо пополнять.
    fn persist_additional_one_time_prekeys(
        &self,
        prekeys: &[omni_crypto::OneTimePrekey],
    ) -> OmniResult<()> {
        if prekeys.is_empty() {
            return Ok(());
        }
        self.with_immediate_transaction("persist_additional_one_time_prekeys", |storage| {
            for prekey in prekeys {
                storage
                    .connection
                    .execute(
                        "
                        INSERT INTO one_time_prekeys (
                            prekey_id,
                            device_id,
                            public_key,
                            private_key_protection_scheme,
                            private_key_blob,
                            suite_version,
                            state,
                            created_at_unix_ms,
                            consumed_at_unix_ms,
                            updated_at_unix_ms
                        )
                        VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'available', ?7, NULL, ?8)
                        ",
                        params![
                            prekey.prekey_id as i64,
                            prekey.device_id.0,
                            prekey_public_key_to_wire(prekey.public_key),
                            prekey.protected_private_key.scheme.as_db_value(),
                            prekey.protected_private_key.blob_hex,
                            prekey.suite_version as i64,
                            prekey.created_at_unix_ms,
                            prekey.updated_at_unix_ms,
                        ],
                    )
                    .map_err(map_sqlite_error)?;
            }
            Ok(())
        })
    }

    fn upsert_contact_dialog_link(
        &self,
        contact_id: &ContactId,
        dialog_key: &str,
        is_primary: bool,
        updated_at_unix_ms: i64,
    ) -> OmniResult<()> {
        let profile_id = self.resolve_transition_profile_id()?;
        self.upsert_contact_dialog_link_for_profile(
            &profile_id,
            contact_id,
            dialog_key,
            is_primary,
            updated_at_unix_ms,
            updated_at_unix_ms,
        )
    }

    fn upsert_contact_dialog_link_for_profile(
        &self,
        profile_id: &LocalProfileId,
        contact_id: &ContactId,
        dialog_key: &str,
        is_primary: bool,
        created_at_unix_ms: i64,
        updated_at_unix_ms: i64,
    ) -> OmniResult<()> {
        let normalized_dialog_key = normalize_dialog_key(dialog_key);

        self.connection
            .execute(
                "
                DELETE FROM contact_dialog_links
                WHERE dialog_key = ?1
                  AND profile_id = ?2
                  AND contact_id <> ?3
                ",
                params![normalized_dialog_key, profile_id.0, contact_id.0],
            )
            .map_err(map_sqlite_error)?;

        if is_primary {
            self.connection
                .execute(
                    "
                    UPDATE contact_dialog_links
                    SET is_primary = 0,
                        updated_at_unix_ms = ?2
                    WHERE contact_id = ?1
                      AND profile_id = ?3
                    ",
                    params![contact_id.0, updated_at_unix_ms, profile_id.0],
                )
                .map_err(map_sqlite_error)?;
        }

        self.connection
            .execute(
                "
                INSERT INTO contact_dialog_links (
                    contact_id,
                    dialog_key,
                    is_primary,
                    created_at_unix_ms,
                    updated_at_unix_ms,
                    profile_id
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ON CONFLICT(contact_id, dialog_key) DO UPDATE SET
                    is_primary = excluded.is_primary,
                    created_at_unix_ms = CASE
                        WHEN contact_dialog_links.created_at_unix_ms < excluded.created_at_unix_ms
                            THEN contact_dialog_links.created_at_unix_ms
                        ELSE excluded.created_at_unix_ms
                    END,
                    updated_at_unix_ms = excluded.updated_at_unix_ms
                ",
                params![
                    contact_id.0,
                    normalized_dialog_key,
                    bool_to_sqlite_integer(is_primary),
                    created_at_unix_ms,
                    updated_at_unix_ms,
                    profile_id.0,
                ],
            )
            .map_err(map_sqlite_error)?;

        Ok(())
    }

    fn execute_migration_batch(&self, step_name: &str, sql: &str) -> OmniResult<()> {
        self.connection.execute_batch(sql).map_err(|error| {
            OmniError::Storage(format!("migration step '{step_name}' failed: {error}"))
        })
    }

    fn ensure_messages_column(&self, column_name: &str, column_sql: &str) -> OmniResult<()> {
        if self.messages_column_exists(column_name)? {
            return Ok(());
        }

        self.execute_migration_batch(
            &format!("messages_add_column_{column_name}"),
            &format!("ALTER TABLE messages ADD COLUMN {column_name} {column_sql};"),
        )
    }

    fn ensure_table_column(
        &self,
        table_name: &str,
        column_name: &str,
        column_sql: &str,
    ) -> OmniResult<()> {
        if self.table_column_exists(table_name, column_name)? {
            return Ok(());
        }

        self.execute_migration_batch(
            &format!("{table_name}_add_column_{column_name}"),
            &format!("ALTER TABLE {table_name} ADD COLUMN {column_name} {column_sql};"),
        )
    }

    fn messages_column_exists(&self, column_name: &str) -> OmniResult<bool> {
        self.table_column_exists("messages", column_name)
    }

    fn table_column_exists(&self, table_name: &str, column_name: &str) -> OmniResult<bool> {
        let mut statement = self
            .connection
            .prepare(&format!("PRAGMA table_info({table_name})"))
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(map_sqlite_error)?;

        for row in rows {
            if row.map_err(map_sqlite_error)? == column_name {
                return Ok(true);
            }
        }

        Ok(false)
    }

    fn get_message_by_id(&self, id: &str) -> OmniResult<ChatMessageRecord> {
        let profile_id = self.resolve_transition_profile_id()?;
        let message = self.connection
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
                WHERE id = ?1
                  AND profile_id = ?2
                ",
                params![id, profile_id.0],
                map_message_row,
            )
            .optional()
            .map_err(map_sqlite_error)?
            .ok_or_else(|| OmniError::Storage(format!("message with id '{id}' was not found")))?;
        self.attach_runtime_info_to_message(message)
    }

    pub fn get_contact(&self, contact_id: &ContactId) -> OmniResult<ContactSnapshot> {
        let profile_id = self.resolve_transition_profile_id()?;
        let contact_id =
            self.resolve_canonical_contact_id_for_profile(&profile_id, contact_id, current_unix_ms())?;
        let contact = self.connection
            .query_row(
                "
                SELECT
                    c.contact_id,
                    c.petname,
                    c.public_display_name,
                    c.public_avatar_hash_hex,
                    c.public_avatar_mime_type,
                    c.public_avatar_pending_hash_hex,
                    c.public_avatar_pending_mime_type,
                    c.remote_instance_id,
                    (
                        SELECT dialog_key
                        FROM contact_dialog_links
                        WHERE contact_id = c.contact_id
                          AND profile_id = c.profile_id
                          AND is_primary = 1
                        ORDER BY updated_at_unix_ms DESC
                        LIMIT 1
                    ) AS primary_dialog_key,
                    c.remote_account_id,
                    c.public_profile_updated_at_unix_ms,
                    c.relationship_state,
                    c.pending_request_direction,
                    c.has_met,
                    c.friend_relay_consent,
                    COALESCE(p.presence_state, 'offline') AS presence_state,
                    p.last_seen_at_unix_ms,
                    p.last_sent_at_unix_ms,
                    p.last_activity_at_unix_ms,
                    c.removed_at_unix_ms,
                    c.created_at_unix_ms,
                    c.updated_at_unix_ms
                FROM contacts c
                LEFT JOIN contact_presence p
                  ON p.contact_id = c.contact_id
                 AND p.profile_id = c.profile_id
                WHERE c.contact_id = ?1
                  AND c.profile_id = ?2
                ",
                params![contact_id.0, profile_id.0],
                map_contact_row,
            )
            .optional()
            .map_err(map_sqlite_error)?
            .ok_or_else(|| OmniError::Storage(format!("contact '{}' was not found", contact_id.0)))?;

        Ok(self.hydrate_contact_public_avatar_image_path(&profile_id, contact))
    }

    fn get_contact_observation(
        &self,
        contact_id: &ContactId,
    ) -> OmniResult<Option<ContactObservationSnapshot>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let identity = self
            .connection
            .query_row(
                "
                SELECT identity_key, observed_display_name, observed_at_unix_ms
                FROM contact_identity_observations
                WHERE contact_id = ?1
                  AND profile_id = ?2
                ",
                params![contact_id.0, profile_id.0],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(map_sqlite_error)?;

        Ok(identity.map(|(identity_key, observed_display_name, observed_at_unix_ms)| {
            ContactObservationSnapshot {
                identity_key: Some(identity_key),
                observed_display_name,
                observed_at_unix_ms,
            }
        }))
    }

    /// The contact's identity as the safety number takes it: the account id — the hash of the
    /// account's master key, which certifies every device of the account — the same at both ends and
    /// on every device (docs/bugs/safety-number-differs-per-own-device.md). A contact whose account is
    /// not known yet has the identity it was observed with.
    fn get_effective_contact_observation(
        &self,
        contact: &ContactSnapshot,
    ) -> OmniResult<Option<ContactObservationSnapshot>> {
        let observed = self.get_contact_observation(&contact.contact_id)?;
        let Some(account_id) = contact.remote_account_id.as_ref() else {
            return Ok(observed);
        };
        Ok(Some(ContactObservationSnapshot {
            identity_key: Some(account_id.0.clone()),
            observed_display_name: observed
                .as_ref()
                .and_then(|observation| observation.observed_display_name.clone())
                .or_else(|| contact.public_display_name.clone()),
            observed_at_unix_ms: observed.map_or(contact.updated_at_unix_ms, |observation| observation.observed_at_unix_ms),
        }))
    }

    /// The contact's devices we know: certified by its account's master key, not revoked.
    fn contact_device_count(&self, contact: &ContactSnapshot) -> usize {
        contact.remote_account_id.as_ref().map_or(0, |account_id| {
            self.list_remote_prekey_bundles_for_account(account_id)
                .map_or(0, |bundles| bundles.len())
        })
    }

    fn get_remote_device_label(
        &self,
        remote_account_id: &AccountId,
        remote_device_id: &DeviceId,
    ) -> OmniResult<Option<String>> {
        self.connection
            .query_row(
                "
                SELECT device_label
                FROM remote_device_observations
                WHERE remote_account_id = ?1
                  AND remote_device_id = ?2
                ORDER BY observed_at_unix_ms DESC
                LIMIT 1
                ",
                params![remote_account_id.0, remote_device_id.0],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(map_sqlite_error)
            .map(|value| value.flatten())
    }

    fn get_contact_verification(
        &self,
        contact_id: &ContactId,
    ) -> OmniResult<Option<VerificationBaseline>> {
        let profile_id = self.resolve_transition_profile_id()?;
        self.connection
            .query_row(
                "
                SELECT
                    verified_at_unix_ms,
                    verification_method,
                    verified_identity_key
                FROM contact_verifications
                WHERE contact_id = ?1
                  AND profile_id = ?2
                ",
                params![contact_id.0, profile_id.0],
                |row| {
                    let method = parse_verification_method(&row.get::<_, String>(1)?)
                        .map_err(into_sqlite_error)?;
                    Ok(VerificationBaseline {
                        verified_at_unix_ms: row.get(0)?,
                        verification_method: method,
                        verified_identity_key: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    #[allow(clippy::too_many_arguments)] // constructor/builder — args are the record's fields
    fn upsert_outbound_queue(
        &self,
        message_id: &str,
        dialog_key: &str,
        status: MessageStatus,
        attempt_count: i64,
        last_attempt_unix_ms: Option<i64>,
        next_attempt_unix_ms: Option<i64>,
        last_failure_kind: Option<QueueFailureKind>,
        last_failure_message: Option<&str>,
        created_at_unix_ms: i64,
        updated_at_unix_ms: i64,
    ) -> OmniResult<()> {
        let remote_delivered_at_unix_ms =
            matches!(status, MessageStatus::Delivered).then_some(updated_at_unix_ms);
        let profile_id = self.resolve_transition_profile_id()?;

        self.connection
            .execute(
                "
                INSERT INTO outbound_queue (
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
                    updated_at_unix_ms,
                    profile_id
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                ON CONFLICT(message_id) DO UPDATE SET
                    dialog_key = excluded.dialog_key,
                    status = excluded.status,
                    attempt_count = excluded.attempt_count,
                    next_attempt_unix_ms = excluded.next_attempt_unix_ms,
                    last_attempt_unix_ms = excluded.last_attempt_unix_ms,
                    last_failure_kind = excluded.last_failure_kind,
                    last_failure_message = excluded.last_failure_message,
                    remote_delivered_at_unix_ms = COALESCE(excluded.remote_delivered_at_unix_ms, outbound_queue.remote_delivered_at_unix_ms),
                    updated_at_unix_ms = excluded.updated_at_unix_ms
                ",
                params![
                    message_id,
                    dialog_key,
                    status_to_db(status),
                    attempt_count,
                    next_attempt_unix_ms,
                    last_attempt_unix_ms,
                    last_failure_kind.map(queue_failure_kind_to_db),
                    last_failure_message,
                    remote_delivered_at_unix_ms,
                    created_at_unix_ms,
                    updated_at_unix_ms,
                    profile_id.0,
                ],
            )
            .map_err(map_sqlite_error)?;

        Ok(())
    }

    fn queue_metadata_for_message(&self, message_id: &str) -> OmniResult<QueueMetadata> {
        let profile_id = self.resolve_transition_profile_id()?;
        self.connection
            .query_row(
                "
                SELECT
                    attempt_count,
                    last_attempt_unix_ms,
                    last_failure_kind,
                    last_failure_message
                FROM outbound_queue
                WHERE message_id = ?1
                  AND profile_id = ?2
                ",
                params![message_id, profile_id.0],
                |row| {
                    let last_failure_kind = row
                        .get::<_, Option<String>>(2)?
                        .map(|value| parse_queue_failure_kind(&value))
                        .transpose()
                        .map_err(into_sqlite_error)?;
                    Ok(QueueMetadata {
                        attempt_count: row.get(0)?,
                        last_attempt_unix_ms: row.get(1)?,
                        last_failure_kind,
                        last_failure_message: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(map_sqlite_error)
            .map(|value| value.unwrap_or_default())
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredProfileSecretsV1 {
    root_secret: omni_identity::AccountRootSecret,
    recovery_phrase_v2: omni_identity::RecoveryPhraseV2,
}

struct ProfileSecretsStorage {
    profile_master_key: omni_crypto::ProtectedSecret,
    encrypted_profile_secrets_json: String,
}

fn build_profile_secrets_storage(
    representation_v2: &omni_identity::AccountRepresentationV2,
) -> OmniResult<ProfileSecretsStorage> {
    seal_profile_secrets(
        &representation_v2.public_identity.account_id,
        &StoredProfileSecretsV1 {
            root_secret: representation_v2.local_secret_state.root_secret.clone(),
            recovery_phrase_v2: representation_v2.local_secret_state.recovery_phrase_v2.clone(),
        },
    )
}

/// The root secret and the phrase under a fresh profile master key, sealed to THIS machine only.
fn seal_profile_secrets(
    account_id: &AccountId,
    profile_secrets: &StoredProfileSecretsV1,
) -> OmniResult<ProfileSecretsStorage> {
    let mut profile_master_key = [0u8; PROFILE_MASTER_KEY_LEN];
    OsRng.fill_bytes(&mut profile_master_key);
    let wrapped_profile_master_key = protect_bytes(&profile_master_key)?;
    let plaintext = serde_json::to_vec(profile_secrets).map_err(map_serde_error_to_storage)?;
    let encrypted = encrypt_at_rest_bytes(
        &profile_master_key,
        format!("omni-profile-secrets-v1|{}", account_id.0).as_bytes(),
        &plaintext,
    )?;
    let encrypted_profile_secrets_json =
        serde_json::to_string(&encrypted).map_err(map_serde_error_to_storage)?;

    Ok(ProfileSecretsStorage {
        profile_master_key: wrapped_profile_master_key,
        encrypted_profile_secrets_json,
    })
}
