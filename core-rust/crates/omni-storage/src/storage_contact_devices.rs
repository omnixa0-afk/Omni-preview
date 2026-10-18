// A contact's devices and what each of them has received
// (docs/decisions/multi-device-equal-nodes.md, step 2.3).
//
// A contact is an account; each of its devices has its own dialog (`dialog:instance:<install>`),
// session and mesh node. The device dialogs hang off the contact as `contact_dialog_links`; the
// device id on a link says whose session encrypts for it. A message to the contact goes to every
// device, and a delivery receipt counts for the device it came from.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactDeviceDialog {
    pub dialog_key: String,
    /// `None` until a session with that dialog's device told us which device it is.
    pub remote_device_id: Option<DeviceId>,
    pub is_primary: bool,
}

impl Storage {
    /// Record which device answers on a dialog of a contact.
    pub fn set_dialog_link_device(&self, dialog_key: &str, device_id: &DeviceId) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "
                UPDATE contact_dialog_links
                SET remote_device_id = ?2
                WHERE dialog_key = ?1 AND profile_id = ?3
                ",
                params![
                    omni_types::normalize_dialog_key(dialog_key),
                    device_id.0.as_str(),
                    profile_id.0.as_str(),
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// When we learned of a contact's device dialog. `None` for a dialog that is not a contact's.
    pub fn contact_dialog_link_known_since(&self, dialog_key: &str) -> OmniResult<Option<i64>> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .query_row(
                "SELECT MIN(created_at_unix_ms) FROM contact_dialog_links WHERE dialog_key = ?1 AND profile_id = ?2",
                params![omni_types::normalize_dialog_key(dialog_key), profile_id.0.as_str()],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)
    }

    /// The device that answers on `dialog_key`: a contact's device, or one of our own other devices.
    /// `None` when the dialog is not tied to a device yet.
    pub fn remote_device_for_dialog(&self, dialog_key: &str) -> OmniResult<Option<DeviceId>> {
        let dialog_key = omni_types::normalize_dialog_key(dialog_key);
        let profile_id = self.require_active_profile_id()?;
        let contact_device: Option<Option<String>> = self
            .connection
            .query_row(
                "SELECT remote_device_id FROM contact_dialog_links WHERE dialog_key = ?1 AND profile_id = ?2",
                params![dialog_key, profile_id.0.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        if let Some(Some(device_id)) = contact_device {
            return Ok(Some(DeviceId(device_id)));
        }
        let own_device: Option<String> = self
            .connection
            .query_row(
                "SELECT device_id FROM account_devices WHERE dialog_key = ?1",
                params![dialog_key],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        Ok(own_device.map(DeviceId))
    }

    /// The session that talks to the device behind `dialog_key`. With several devices per account
    /// the account's latest session may belong to ANOTHER device: encrypting with it and sending on
    /// this dialog reaches a device that cannot decrypt (docs/decisions/multi-device-equal-nodes.md).
    /// The latest session stands in only for a dialog not tied to a device yet.
    pub fn get_session_for_dialog(
        &self,
        remote_account_id: &AccountId,
        dialog_key: &str,
    ) -> OmniResult<Option<SessionRecord>> {
        match self.remote_device_for_dialog(dialog_key)? {
            Some(device_id) => self.get_session_record(&SessionAddress {
                remote_account_id: remote_account_id.clone(),
                remote_device_id: device_id,
            }),
            None => self.get_latest_session_for_remote_account(remote_account_id),
        }
    }

    /// The contact's device dialogs, primary first.
    pub fn list_contact_device_dialogs(
        &self,
        contact_id: &ContactId,
    ) -> OmniResult<Vec<ContactDeviceDialog>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT dialog_key, remote_device_id, is_primary
                FROM contact_dialog_links
                WHERE contact_id = ?1 AND profile_id = ?2
                  AND (
                      remote_device_id IS NULL
                      OR NOT EXISTS (
                          SELECT 1 FROM device_revocations
                          WHERE device_id = contact_dialog_links.remote_device_id
                            AND account_id = (
                                SELECT remote_account_id FROM contacts
                                WHERE contact_id = contact_dialog_links.contact_id
                            )
                      )
                  )
                ORDER BY is_primary DESC, created_at_unix_ms ASC, dialog_key ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![contact_id.0.as_str(), profile_id.0.as_str()], |row| {
                Ok(ContactDeviceDialog {
                    dialog_key: row.get(0)?,
                    remote_device_id: row.get::<_, Option<String>>(1)?.map(DeviceId),
                    is_primary: row.get::<_, i64>(2)? != 0,
                })
            })
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    /// Every device of every approved contact we hold a bundle for, as (account, the device's
    /// identity signing key — its mesh node): one query for the loops that keep a link with all of
    /// them. Walking the contacts and asking for each one's bundles was two queries per contact
    /// every tick. Contacts by name, each contact's devices newest first.
    pub fn list_approved_contact_device_signing_keys(
        &self,
    ) -> OmniResult<Vec<(AccountId, omni_crypto::IdentitySigningPublicKey)>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT c.remote_account_id, b.identity_signing_public_key
                FROM contacts c
                JOIN remote_prekey_bundles b ON b.remote_account_id = c.remote_account_id
                WHERE c.profile_id = ?1
                  AND c.removed_at_unix_ms IS NULL
                  AND c.relationship_state = ?2
                  AND NOT EXISTS (
                      SELECT 1 FROM device_revocations r
                      WHERE r.account_id = b.remote_account_id
                        AND r.device_id = b.remote_device_id
                  )
                ORDER BY c.petname COLLATE NOCASE ASC, c.contact_id ASC, b.observed_at_unix_ms DESC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(
                params![
                    profile_id.0.as_str(),
                    omni_identity::ContactRelationshipState::Approved.as_db_value()
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(map_sqlite_error)?;
        let mut keys = Vec::new();
        for row in rows {
            let (account_id, signing_key) = row.map_err(map_sqlite_error)?;
            // One unreadable row must not cost every other contact its link.
            if let Ok(key) = identity_signing_public_key_from_wire(&signing_key) {
                keys.push((AccountId(account_id), key));
            }
        }
        Ok(keys)
    }

    /// The prekey bundle of every device of `account_id` we know — one per device.
    pub fn list_remote_prekey_bundles_for_account(
        &self,
        account_id: &AccountId,
    ) -> OmniResult<Vec<PrekeyBundle>> {
        let device_ids: Vec<String> = {
            let mut statement = self
                .connection
                .prepare(
                    "
                    SELECT remote_device_id FROM remote_prekey_bundles
                    WHERE remote_account_id = ?1
                      AND NOT EXISTS (
                          SELECT 1 FROM device_revocations
                          WHERE account_id = remote_prekey_bundles.remote_account_id
                            AND device_id = remote_prekey_bundles.remote_device_id
                      )
                    ORDER BY observed_at_unix_ms DESC
                    ",
                )
                .map_err(map_sqlite_error)?;
            let rows = statement
                .query_map(params![account_id.0.as_str()], |row| row.get::<_, String>(0))
                .map_err(map_sqlite_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)?
        };
        let mut bundles = Vec::new();
        for device_id in device_ids {
            if let Some(bundle) = self.get_remote_prekey_bundle(account_id, &DeviceId(device_id))? {
                bundles.push(bundle);
            }
        }
        Ok(bundles)
    }

    pub fn record_message_device_receipt(
        &self,
        message_id: &str,
        device_id: &DeviceId,
        delivered_at_unix_ms: i64,
    ) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "
                INSERT INTO message_device_receipts (
                    message_id, remote_device_id, delivered_at_unix_ms, profile_id
                )
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(message_id, remote_device_id) DO NOTHING
                ",
                params![message_id, device_id.0.as_str(), delivered_at_unix_ms, profile_id.0.as_str()],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// Outgoing chat messages of `dialog_key` since `since_unix_ms` that already left the queue
    /// (sent or delivered) — what a device that was away may still be missing. Messages only: call
    /// history and other synthetic entries are not in this table.
    pub fn list_sent_messages_since(
        &self,
        dialog_key: &str,
        since_unix_ms: i64,
    ) -> OmniResult<Vec<ChatMessageRecord>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT id, dialog_key, direction, body, status,
                       remote_delivered_at_unix_ms, created_at_unix_ms, updated_at_unix_ms
                FROM messages
                WHERE profile_id = ?1
                  AND dialog_key = ?2
                  AND direction = 'outgoing'
                  AND status IN ('sent', 'delivered')
                  AND created_at_unix_ms >= ?3
                ORDER BY created_at_unix_ms ASC, id ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(
                params![
                    profile_id.0.as_str(),
                    omni_types::normalize_dialog_key(dialog_key),
                    since_unix_ms
                ],
                map_message_row,
            )
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    /// The devices that confirmed `message_id`.
    pub fn message_delivered_devices(&self, message_id: &str) -> OmniResult<Vec<DeviceId>> {
        let mut statement = self
            .connection
            .prepare("SELECT remote_device_id FROM message_device_receipts WHERE message_id = ?1")
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![message_id], |row| row.get::<_, String>(0).map(DeviceId))
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }
}
