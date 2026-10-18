// A copied profile folder becomes a NEW device of the same account
// (docs/decisions/multi-device-equal-nodes.md, step 1.4).
//
// The folder is portable on purpose, so the same database can wake up somewhere it was not born:
// on another machine its device keys do not open (they are sealed by DPAPI to the machine and
// user), on the same machine it sits at another path. Running there with the old device identity
// would make two nodes with one address and two copies of one ratchet state — the clone this
// design exists to prevent. Instead the copy keeps the account, contacts and history, drops what
// belonged to the old device (its keys, its sessions, its MLS state) and gets a fresh device id;
// building the crypto foundation then gives it new keys and a certificate from the working key.

/// How this database's location is remembered: canonical and, on Windows, case-folded (the file
/// system is case-insensitive, so `D:\Omni` and `d:\omni` are one place).
fn device_binding_path(db_path: &Path) -> String {
    let canonical = fs::canonicalize(db_path).unwrap_or_else(|_| db_path.to_path_buf());
    let text = canonical.to_string_lossy().to_string();
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

impl Storage {
    /// Whether the current device's identity belongs to another place. Binds a device that has no
    /// recorded location yet (a profile from before this check) to the current one.
    fn local_device_was_copied(
        &self,
        account_id: &AccountId,
        device: &LocalDeviceSnapshot,
    ) -> OmniResult<bool> {
        let here = device_binding_path(self.db_path());
        let bound: Option<String> = self
            .connection
            .query_row(
                "SELECT bound_db_path FROM local_devices WHERE device_id = ?1",
                params![device.device_id.0.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_error)?
            .flatten();
        match bound {
            Some(bound) if bound != here => return Ok(true),
            Some(_) => {}
            None => self.bind_device_to_path(&device.device_id, &here)?,
        }
        if let Some(identity) = self.get_local_identity_key(account_id)? {
            if unprotect_bytes(&identity.protected_signing_private_key).is_err() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn bind_device_to_path(&self, device_id: &DeviceId, path: &str) -> OmniResult<()> {
        self.connection
            .execute(
                "UPDATE local_devices SET bound_db_path = ?2 WHERE device_id = ?1",
                params![device_id.0.as_str(), path],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// A copied folder inherits the original's label, and two devices of one account then read the
    /// same in the device list. Name the copy "<label> (копия)", numbered when that is taken too.
    fn label_for_copied_device(&self, account_id: &AccountId, original_label: &str) -> OmniResult<String> {
        let base = original_label
            .rfind(" (копия")
            .map_or(original_label, |position| &original_label[..position]);
        let taken: Vec<String> = self
            .list_account_devices(account_id)?
            .into_iter()
            .map(|record| record.device_label)
            .collect();
        let first = format!("{base} (копия)");
        if !taken.contains(&first) {
            return Ok(first);
        }
        let mut number = 2;
        loop {
            let candidate = format!("{base} (копия {number})");
            if !taken.contains(&candidate) {
                return Ok(candidate);
            }
            number += 1;
        }
    }

    /// Retire the copied device's identity here and become a new device of the same account.
    /// The old device stays in the account's device list: the original folder may still be alive.
    fn become_new_device(
        &self,
        account_id: &AccountId,
        old_device: &LocalDeviceSnapshot,
        now_unix_ms: i64,
    ) -> OmniResult<LocalDeviceSnapshot> {
        let new_device = LocalDeviceSnapshot {
            device_id: omni_identity::new_device_id(),
            account_id: account_id.clone(),
            device_label: self.label_for_copied_device(account_id, &old_device.device_label)?,
            is_current: true,
            restored_from_recovery: old_device.restored_from_recovery,
            created_at_unix_ms: now_unix_ms,
            updated_at_unix_ms: now_unix_ms,
        };
        let here = device_binding_path(self.db_path());
        self.with_immediate_transaction("become_new_device", |storage| {
            storage
                .connection
                .execute(
                    "UPDATE local_devices SET is_current = 0, updated_at_unix_ms = ?2 WHERE device_id = ?1",
                    params![old_device.device_id.0.as_str(), now_unix_ms],
                )
                .map_err(map_sqlite_error)?;
            storage
                .connection
                .execute(
                    "
                    INSERT INTO local_devices (
                        device_id, account_id, device_label, is_current, restored_from_recovery,
                        created_at_unix_ms, updated_at_unix_ms, bound_db_path
                    )
                    VALUES (?1, ?2, ?3, 1, ?4, ?5, ?5, ?6)
                    ",
                    params![
                        new_device.device_id.0.as_str(),
                        account_id.0.as_str(),
                        new_device.device_label.as_str(),
                        bool_to_sqlite_integer(new_device.restored_from_recovery),
                        now_unix_ms,
                        here.as_str(),
                    ],
                )
                .map_err(map_sqlite_error)?;
            storage
                .connection
                .execute(
                    "UPDATE profile_current_device SET device_id = ?1, updated_at_unix_ms = ?2 WHERE device_id = ?3",
                    params![
                        new_device.device_id.0.as_str(),
                        now_unix_ms,
                        old_device.device_id.0.as_str(),
                    ],
                )
                .map_err(map_sqlite_error)?;
            // What only the old device could use: its keys and the sessions built on them.
            for (sql, key) in [
                ("DELETE FROM one_time_prekeys WHERE device_id = ?1", &old_device.device_id.0),
                ("DELETE FROM signed_prekeys WHERE device_id = ?1", &old_device.device_id.0),
                ("DELETE FROM device_keys WHERE device_id = ?1", &old_device.device_id.0),
                ("DELETE FROM identity_keys WHERE account_id = ?1", &account_id.0),
            ] {
                storage
                    .connection
                    .execute(sql, params![key.as_str()])
                    .map_err(map_sqlite_error)?;
            }
            let profile_id = storage.get_active_profile_id()?.ok_or_else(|| {
                OmniError::Storage("no active profile for the copied device".to_string())
            })?;
            // The messages the folder already holds were sent by the device it was copied from, and
            // every device that existed then has them: the new device must not hand them back as
            // "copies" (an echo to their author, once per message, for the whole catch-up window).
            // One still `sending` counts too: it already left (the peer may have it before the row
            // turns `sent`); only a queued one that never went out stays owed.
            storage
                .connection
                .execute(
                    "
                    INSERT OR IGNORE INTO message_device_receipts (
                        message_id, remote_device_id, delivered_at_unix_ms, profile_id
                    )
                    SELECT m.id, d.device_id, ?3, ?1
                    FROM messages m
                    JOIN account_devices d ON d.account_id = ?2
                    WHERE m.profile_id = ?1
                      AND m.direction = 'outgoing'
                      AND m.copied_from_device_id IS NULL
                      AND m.status IN ('sending', 'sent', 'delivered')
                      AND d.device_id <> ?4
                    ",
                    params![
                        profile_id.0.as_str(),
                        account_id.0.as_str(),
                        now_unix_ms,
                        new_device.device_id.0.as_str(),
                    ],
                )
                .map_err(map_sqlite_error)?;
            for table in ["replay_windows", "sessions"] {
                storage
                    .connection
                    .execute(
                        &format!("DELETE FROM {table} WHERE profile_id = ?1"),
                        params![profile_id.0.as_str()],
                    )
                    .map_err(map_sqlite_error)?;
            }
            storage.wipe_mls_state()?;
            // A folder under a password: the new device is certified by the working key, which only
            // the password opens. The copy was saved signed in, so nothing would ever ask for it —
            // it starts locked instead.
            if storage
                .get_profile_secrets_row(&profile_id)?
                .is_some_and(|row| row.password_enabled)
            {
                storage.update_profile_state(
                    &profile_id,
                    omni_identity::LocalProfileState::SignedOutLocked,
                    now_unix_ms,
                )?;
            }
            // The installation id names this node to the transport; the copy is another node.
            storage
                .connection
                .execute(
                    "DELETE FROM app_meta WHERE key = ?1",
                    params![LOCAL_INSTANCE_ID_META_KEY],
                )
                .map_err(map_sqlite_error)?;
            Ok(())
        })?;
        Ok(new_device)
    }
}
