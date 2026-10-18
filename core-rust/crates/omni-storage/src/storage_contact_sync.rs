// Contacts shared by the account's devices (docs/decisions/multi-device-equal-nodes.md,
// «Синхронизация контактов»).
//
// What is shared is STATE, not a log: one record per contact account — relationship (approved /
// blocked / removed) and name, each with its own version counter — and a removal stays as a
// tombstone. A device that was away for any time catches up from whichever of our devices it meets,
// and a stale device cannot bring a removed contact back: its record is older and loses.
//
// Local changes are not hooked path by path (a contact gets approved by a button, an invite, an
// approval signal, a search by Omni-ID…): `refresh_contact_sync_records` compares the contacts
// table with the records and turns every difference into a new version written by this device.
// What a merge applies goes to the contacts table in the same transaction as its record, so the
// next refresh sees no difference.

/// The relationship part of a contact record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContactSyncState {
    Approved,
    Blocked,
    Removed,
}

impl ContactSyncState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Blocked => "blocked",
            Self::Removed => "removed",
        }
    }

    pub fn parse(value: &str) -> OmniResult<Self> {
        match value {
            "approved" => Ok(Self::Approved),
            "blocked" => Ok(Self::Blocked),
            "removed" => Ok(Self::Removed),
            other => Err(OmniError::InvalidInput(format!("contact sync state '{other}' is unknown"))),
        }
    }

    /// The tie-break between two changes made with the same version (made without seeing each
    /// other): a removal beats a block beats an approval — the more deliberate step stands.
    fn rank(self) -> u8 {
        match self {
            Self::Approved => 0,
            Self::Blocked => 1,
            Self::Removed => 2,
        }
    }

    /// What a contacts-table row says, or `None` for a relationship that is this device's own
    /// business (a pending request).
    fn of_contact(contact: &ContactSnapshot) -> Option<Self> {
        match contact.relationship_state {
            omni_identity::ContactRelationshipState::Approved => Some(Self::Approved),
            omni_identity::ContactRelationshipState::Declined => Some(Self::Blocked),
            omni_identity::ContactRelationshipState::Pending => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactSyncRecord {
    pub account_id: AccountId,
    pub state: ContactSyncState,
    pub state_version: u64,
    pub state_writer: String,
    pub petname: String,
    pub petname_version: u64,
    pub petname_writer: String,
    pub display_name: Option<String>,
    pub seq: u64,
}

impl ContactSyncRecord {
    fn state_key(&self) -> (u64, u8, &str) {
        (self.state_version, self.state.rank(), self.state_writer.as_str())
    }

    fn petname_key(&self) -> (u64, &str) {
        (self.petname_version, self.petname_writer.as_str())
    }
}

/// What a merged entry changed, for the contacts table: the relationship when it won, the name when
/// it won.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactSyncMerge {
    pub account_id: AccountId,
    pub state: Option<ContactSyncState>,
    pub petname: Option<String>,
    pub display_name: Option<String>,
}

/// How far one of our devices has come with this device's changes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContactSyncMark {
    pub acked_seq: u64,
    pub sent_through_seq: u64,
    pub sent_at_unix_ms: i64,
}

impl Storage {
    /// Turn every difference between the contacts table and the shared records into a new version
    /// written by `local_device_id`. Returns how many records changed.
    pub fn refresh_contact_sync_records(
        &self,
        local_device_id: &DeviceId,
        now_unix_ms: i64,
    ) -> OmniResult<usize> {
        let profile_id = self.require_active_profile_id()?;
        let own_account_id = self.get_local_account()?.map(|account| account.descriptor.account_id);
        let writer = local_device_id.0.as_str();
        self.with_immediate_transaction("refresh_contact_sync_records", |storage| {
            let records: std::collections::HashMap<AccountId, ContactSyncRecord> = storage
                .list_contact_sync_records_after(0, i64::MAX as u64)?
                .into_iter()
                .map(|record| (record.account_id.clone(), record))
                .collect();
            let mut present: std::collections::HashSet<AccountId> = std::collections::HashSet::new();
            let mut changed = 0;
            for contact in storage.list_contacts()? {
                let Some(account_id) = contact.remote_account_id.clone() else {
                    continue;
                };
                if own_account_id.as_ref() == Some(&account_id) || !present.insert(account_id.clone()) {
                    continue;
                }
                let Some(state) = ContactSyncState::of_contact(&contact) else {
                    continue;
                };
                let next = match records.get(&account_id) {
                    None => ContactSyncRecord {
                        account_id: account_id.clone(),
                        state,
                        state_version: 1,
                        state_writer: writer.to_string(),
                        petname: contact.petname.clone(),
                        petname_version: 1,
                        petname_writer: writer.to_string(),
                        display_name: contact.public_display_name.clone(),
                        seq: 0,
                    },
                    Some(record) => {
                        let mut next = record.clone();
                        if record.state != state {
                            next.state = state;
                            next.state_version = record.state_version + 1;
                            next.state_writer = writer.to_string();
                        }
                        if record.petname != contact.petname {
                            next.petname = contact.petname.clone();
                            next.petname_version = record.petname_version + 1;
                            next.petname_writer = writer.to_string();
                        }
                        if next == *record {
                            continue;
                        }
                        if next.display_name.is_none() {
                            next.display_name = contact.public_display_name.clone();
                        }
                        next
                    }
                };
                storage.write_contact_sync_record(&profile_id, next, now_unix_ms)?;
                changed += 1;
            }
            // A contact this device had and no longer has was removed here.
            for record in records.values() {
                if record.state == ContactSyncState::Removed || present.contains(&record.account_id) {
                    continue;
                }
                let mut next = record.clone();
                next.state = ContactSyncState::Removed;
                next.state_version = record.state_version + 1;
                next.state_writer = writer.to_string();
                storage.write_contact_sync_record(&profile_id, next, now_unix_ms)?;
                changed += 1;
            }
            Ok(changed)
        })
    }

    /// Fold one record from another of our devices in. `None` when it brings nothing newer; otherwise
    /// the record is stored and the result says what the contacts table has to follow. Call it inside
    /// the transaction that applies that, or a crash in between reads as a local change.
    pub fn merge_contact_sync_record(
        &self,
        incoming: &ContactSyncRecord,
        now_unix_ms: i64,
    ) -> OmniResult<Option<ContactSyncMerge>> {
        let profile_id = self.require_active_profile_id()?;
        let local = self.get_contact_sync_record(&incoming.account_id)?;
        let state_wins = local
            .as_ref()
            .is_none_or(|local| incoming.state_key() > local.state_key());
        let petname_wins = local
            .as_ref()
            .is_none_or(|local| incoming.petname_key() > local.petname_key());
        if !state_wins && !petname_wins {
            return Ok(None);
        }
        let mut next = match &local {
            Some(local) => local.clone(),
            None => incoming.clone(),
        };
        if state_wins {
            next.state = incoming.state;
            next.state_version = incoming.state_version;
            next.state_writer = incoming.state_writer.clone();
        }
        if petname_wins {
            next.petname = incoming.petname.clone();
            next.petname_version = incoming.petname_version;
            next.petname_writer = incoming.petname_writer.clone();
        }
        if next.display_name.is_none() {
            next.display_name = incoming.display_name.clone();
        }
        let merge = ContactSyncMerge {
            account_id: incoming.account_id.clone(),
            state: state_wins.then_some(next.state),
            petname: petname_wins.then(|| next.petname.clone()),
            display_name: next.display_name.clone(),
        };
        self.write_contact_sync_record(&profile_id, next, now_unix_ms)?;
        Ok(Some(merge))
    }

    pub fn get_contact_sync_record(&self, account_id: &AccountId) -> OmniResult<Option<ContactSyncRecord>> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .query_row(
                "
                SELECT remote_account_id, state, state_version, state_writer, petname,
                       petname_version, petname_writer, display_name, seq
                FROM contact_sync_records
                WHERE profile_id = ?1 AND remote_account_id = ?2
                ",
                params![profile_id.0.as_str(), account_id.0.as_str()],
                map_contact_sync_row,
            )
            .optional()
            .map_err(map_sqlite_error)?
            .map(contact_sync_record_from_row)
            .transpose()
    }

    /// This device's records changed after `after_seq`, oldest change first, at most `limit`.
    pub fn list_contact_sync_records_after(
        &self,
        after_seq: u64,
        limit: u64,
    ) -> OmniResult<Vec<ContactSyncRecord>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT remote_account_id, state, state_version, state_writer, petname,
                       petname_version, petname_writer, display_name, seq
                FROM contact_sync_records
                WHERE profile_id = ?1 AND seq > ?2
                ORDER BY seq ASC
                LIMIT ?3
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(
                params![
                    profile_id.0.as_str(),
                    after_seq.min(i64::MAX as u64) as i64,
                    limit.min(i64::MAX as u64) as i64
                ],
                map_contact_sync_row,
            )
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)?;
        rows.into_iter().map(contact_sync_record_from_row).collect()
    }

    /// Blocks and removals this device learned of since `since_unix_ms` — its own or merged from
    /// another device of ours — newest first, at most `limit`: what goes in the account's mailbox for
    /// a device that is offline now.
    pub fn list_recent_contact_sync_blocks_and_removals(
        &self,
        since_unix_ms: i64,
        limit: u64,
    ) -> OmniResult<Vec<ContactSyncRecord>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT remote_account_id, state, state_version, state_writer, petname,
                       petname_version, petname_writer, display_name, seq
                FROM contact_sync_records
                WHERE profile_id = ?1 AND state IN ('blocked', 'removed') AND updated_at_unix_ms >= ?2
                ORDER BY updated_at_unix_ms DESC, seq DESC
                LIMIT ?3
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(
                params![profile_id.0.as_str(), since_unix_ms, limit.min(i64::MAX as u64) as i64],
                map_contact_sync_row,
            )
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)?;
        rows.into_iter().map(contact_sync_record_from_row).collect()
    }

    pub fn contact_sync_mark(&self, device_id: &DeviceId) -> OmniResult<ContactSyncMark> {
        let profile_id = self.require_active_profile_id()?;
        Ok(self
            .connection
            .query_row(
                "
                SELECT acked_seq, sent_through_seq, sent_at_unix_ms
                FROM contact_sync_device_marks
                WHERE profile_id = ?1 AND device_id = ?2
                ",
                params![profile_id.0.as_str(), device_id.0.as_str()],
                |row| {
                    Ok(ContactSyncMark {
                        acked_seq: row.get::<_, i64>(0)?.max(0) as u64,
                        sent_through_seq: row.get::<_, i64>(1)?.max(0) as u64,
                        sent_at_unix_ms: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(map_sqlite_error)?
            .unwrap_or_default())
    }

    pub fn record_contact_sync_sent(
        &self,
        device_id: &DeviceId,
        through_seq: u64,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "
                INSERT INTO contact_sync_device_marks (profile_id, device_id, sent_through_seq, sent_at_unix_ms)
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(profile_id, device_id) DO UPDATE SET
                    sent_through_seq = excluded.sent_through_seq,
                    sent_at_unix_ms = excluded.sent_at_unix_ms
                ",
                params![
                    profile_id.0.as_str(),
                    device_id.0.as_str(),
                    through_seq.min(i64::MAX as u64) as i64,
                    now_unix_ms
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// `device_id` has applied this device's changes up to `through_seq`. Never moves back.
    pub fn record_contact_sync_acked(&self, device_id: &DeviceId, through_seq: u64) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "
                INSERT INTO contact_sync_device_marks (profile_id, device_id, acked_seq)
                VALUES (?1, ?2, ?3)
                ON CONFLICT(profile_id, device_id) DO UPDATE SET
                    acked_seq = MAX(acked_seq, excluded.acked_seq)
                ",
                params![
                    profile_id.0.as_str(),
                    device_id.0.as_str(),
                    through_seq.min(i64::MAX as u64) as i64
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// Store `record` as this device's newest change: it gets the next `seq`, so each of our devices
    /// is handed it once more.
    fn write_contact_sync_record(
        &self,
        profile_id: &LocalProfileId,
        record: ContactSyncRecord,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        let next_seq: i64 = self
            .connection
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM contact_sync_records WHERE profile_id = ?1",
                params![profile_id.0.as_str()],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                INSERT INTO contact_sync_records (
                    profile_id, remote_account_id, state, state_version, state_writer, petname,
                    petname_version, petname_writer, display_name, seq, updated_at_unix_ms
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                ON CONFLICT(profile_id, remote_account_id) DO UPDATE SET
                    state = excluded.state,
                    state_version = excluded.state_version,
                    state_writer = excluded.state_writer,
                    petname = excluded.petname,
                    petname_version = excluded.petname_version,
                    petname_writer = excluded.petname_writer,
                    display_name = excluded.display_name,
                    seq = excluded.seq,
                    updated_at_unix_ms = excluded.updated_at_unix_ms
                ",
                params![
                    profile_id.0.as_str(),
                    record.account_id.0.as_str(),
                    record.state.as_str(),
                    record.state_version.min(i64::MAX as u64) as i64,
                    record.state_writer,
                    record.petname,
                    record.petname_version.min(i64::MAX as u64) as i64,
                    record.petname_writer,
                    record.display_name,
                    next_seq,
                    now_unix_ms,
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }
}

type ContactSyncRow = (String, String, i64, String, String, i64, String, Option<String>, i64);

fn map_contact_sync_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ContactSyncRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
    ))
}

fn contact_sync_record_from_row(row: ContactSyncRow) -> OmniResult<ContactSyncRecord> {
    let (account_id, state, state_version, state_writer, petname, petname_version, petname_writer, display_name, seq) =
        row;
    Ok(ContactSyncRecord {
        account_id: AccountId(account_id),
        state: ContactSyncState::parse(&state)?,
        state_version: state_version.max(0) as u64,
        state_writer,
        petname,
        petname_version: petname_version.max(0) as u64,
        petname_writer,
        display_name,
        seq: seq.max(0) as u64,
    })
}
