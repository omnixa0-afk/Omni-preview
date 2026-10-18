// Standing remote access between us and a contact (docs/decisions/remote-access-for-contacts.md).
// One row per contact, both directions: what WE let the contact do to this computer (the decision
// every session is checked against) and what the contact told us it lets us do to theirs (only
// shown — the contact's own core decides). Kept apart from `contacts`: an access grant is not
// identity or relationship state.

/// Remote access with one contact, both directions.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContactRemoteAccessRecord {
    pub contact_id: ContactId,
    /// We let the contact control this computer; `None` — we don't.
    pub granted_mode: Option<omni_types::RemoteAccessMode>,
    /// When our side last changed (grant or revocation) — what the contact must hear about.
    pub granted_changed_at_unix_ms: Option<i64>,
    /// The contact lets us control theirs (as the contact last told us).
    pub received_mode: Option<omni_types::RemoteAccessMode>,
    pub received_changed_at_unix_ms: Option<i64>,
    pub last_session_at_unix_ms: Option<i64>,
}

impl ContactRemoteAccessRecord {
    fn empty(contact_id: ContactId) -> Self {
        Self {
            contact_id,
            granted_mode: None,
            granted_changed_at_unix_ms: None,
            received_mode: None,
            received_changed_at_unix_ms: None,
            last_session_at_unix_ms: None,
        }
    }
}

impl Storage {
    pub fn get_contact_remote_access(&self, contact_id: &ContactId) -> OmniResult<ContactRemoteAccessRecord> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .query_row(
                "SELECT contact_id, granted_mode, granted_changed_at_unix_ms, received_mode,
                        received_changed_at_unix_ms, last_session_at_unix_ms
                 FROM contact_remote_access WHERE profile_id = ?1 AND contact_id = ?2",
                params![profile_id.0, contact_id.0],
                remote_access_from_row,
            )
            .optional()
            .map_err(map_sqlite_error)
            .map(|row| row.unwrap_or_else(|| ContactRemoteAccessRecord::empty(contact_id.clone())))
    }

    /// Every contact with any remote-access history, either direction.
    pub fn list_contact_remote_access(&self) -> OmniResult<Vec<ContactRemoteAccessRecord>> {
        let profile_id = self.require_active_profile_id()?;
        let mut stmt = self
            .connection
            .prepare(
                "SELECT contact_id, granted_mode, granted_changed_at_unix_ms, received_mode,
                        received_changed_at_unix_ms, last_session_at_unix_ms
                 FROM contact_remote_access WHERE profile_id = ?1 ORDER BY contact_id",
            )
            .map_err(map_sqlite_error)?;
        let rows = stmt
            .query_map(params![profile_id.0], remote_access_from_row)
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    /// Our side: let the contact control this computer (`Some(mode)`) or stop (`None`).
    pub fn set_contact_remote_access_granted(
        &self,
        contact_id: &ContactId,
        mode: Option<omni_types::RemoteAccessMode>,
        now_unix_ms: i64,
    ) -> OmniResult<ContactRemoteAccessRecord> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "INSERT INTO contact_remote_access (profile_id, contact_id, granted_mode, granted_changed_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (profile_id, contact_id) DO UPDATE SET
                    granted_mode = excluded.granted_mode,
                    granted_changed_at_unix_ms = excluded.granted_changed_at_unix_ms",
                params![profile_id.0, contact_id.0, mode.map(|m| m.as_db_value()), now_unix_ms],
            )
            .map_err(map_sqlite_error)?;
        self.get_contact_remote_access(contact_id)
    }

    /// The contact's side, as it told us. An older message than the one we hold is ignored, so a
    /// grant re-sent on reconnect cannot undo a newer revocation that overtook it.
    pub fn set_contact_remote_access_received(
        &self,
        contact_id: &ContactId,
        mode: Option<omni_types::RemoteAccessMode>,
        sent_at_unix_ms: i64,
    ) -> OmniResult<ContactRemoteAccessRecord> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "INSERT INTO contact_remote_access (profile_id, contact_id, received_mode, received_changed_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (profile_id, contact_id) DO UPDATE SET
                    received_mode = excluded.received_mode,
                    received_changed_at_unix_ms = excluded.received_changed_at_unix_ms
                 WHERE contact_remote_access.received_changed_at_unix_ms IS NULL
                    OR contact_remote_access.received_changed_at_unix_ms < excluded.received_changed_at_unix_ms",
                params![profile_id.0, contact_id.0, mode.map(|m| m.as_db_value()), sent_at_unix_ms],
            )
            .map_err(map_sqlite_error)?;
        self.get_contact_remote_access(contact_id)
    }

    pub fn note_contact_remote_access_session(&self, contact_id: &ContactId, now_unix_ms: i64) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "UPDATE contact_remote_access SET last_session_at_unix_ms = ?3
                 WHERE profile_id = ?1 AND contact_id = ?2",
                params![profile_id.0, contact_id.0, now_unix_ms],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }
}

fn remote_access_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ContactRemoteAccessRecord> {
    let mode = |value: Option<String>| value.as_deref().and_then(omni_types::RemoteAccessMode::from_db_value);
    Ok(ContactRemoteAccessRecord {
        contact_id: ContactId(row.get(0)?),
        granted_mode: mode(row.get(1)?),
        granted_changed_at_unix_ms: row.get(2)?,
        received_mode: mode(row.get(3)?),
        received_changed_at_unix_ms: row.get(4)?,
        last_session_at_unix_ms: row.get(5)?,
    })
}
