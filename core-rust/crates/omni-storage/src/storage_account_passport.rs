// The account passport at rest (docs/decisions/account-recovery-from-friends.md, Б1).
//
// Three sides of one mechanism, each in its own table:
// - `own_passport`: the last passport this device built and sealed — what goes to the holders;
// - `passport_holders`: the contacts this device chose to keep it, and what each confirmed;
// - `held_passports`: passports other accounts handed us to keep. Opaque blobs: we cannot read
//   them, we only keep the newest one per account and hand it back to that account's devices.
// What a device restored from the phrase brought back lives in `app_meta` (the recovery job) and in
// `recovered_groups` (groups it was in, for rejoining).

/// Everything we keep for others together stays under this, so a contact cannot fill our disk.
pub const HELD_PASSPORTS_MAX_TOTAL_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnPassportRecord {
    pub version: i64,
    /// What the passport says, without the time it was written: an unchanged passport is not
    /// rebuilt and sent again.
    pub fingerprint: String,
    pub sealed: Vec<u8>,
    pub built_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassportHolderRecord {
    pub account_id: AccountId,
    pub chosen_at_unix_ms: i64,
    pub sent_version: i64,
    pub sent_at_unix_ms: i64,
    pub confirmed_version: i64,
    pub confirmed_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldPassportRecord {
    pub owner_account_id: AccountId,
    pub locator_hex: String,
    pub version: i64,
    pub sealed: Vec<u8>,
    pub mark_secret_hex: String,
    pub stored_at_unix_ms: i64,
}

/// Why a passport was not taken to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldPassportOutcome {
    Stored,
    /// We already keep this version or a newer one (nothing changed; still worth an answer).
    AlreadyNewer,
    /// It would not fit in [`HELD_PASSPORTS_MAX_TOTAL_BYTES`].
    OverQuota,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredGroupRecord {
    pub group_id: String,
    pub name: String,
    pub role: String,
    pub recovered_at_unix_ms: i64,
}

impl Storage {
    pub fn get_own_passport(&self) -> OmniResult<Option<OwnPassportRecord>> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .query_row(
                "SELECT version, fingerprint, sealed, built_at_unix_ms FROM own_passport WHERE profile_id = ?1",
                params![profile_id.0.as_str()],
                |row| {
                    Ok(OwnPassportRecord {
                        version: row.get(0)?,
                        fingerprint: row.get(1)?,
                        sealed: row.get(2)?,
                        built_at_unix_ms: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    pub fn store_own_passport(&self, record: &OwnPassportRecord) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "
                INSERT INTO own_passport (profile_id, version, fingerprint, sealed, built_at_unix_ms)
                VALUES (?1, ?2, ?3, ?4, ?5)
                ON CONFLICT(profile_id) DO UPDATE SET
                    version = excluded.version,
                    fingerprint = excluded.fingerprint,
                    sealed = excluded.sealed,
                    built_at_unix_ms = excluded.built_at_unix_ms
                ",
                params![
                    profile_id.0.as_str(),
                    record.version,
                    record.fingerprint,
                    record.sealed,
                    record.built_at_unix_ms
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn list_passport_holders(&self) -> OmniResult<Vec<PassportHolderRecord>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT account_id, chosen_at_unix_ms, sent_version, sent_at_unix_ms,
                       confirmed_version, confirmed_at_unix_ms
                FROM passport_holders WHERE profile_id = ?1
                ORDER BY chosen_at_unix_ms ASC, account_id ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0.as_str()], |row| {
                Ok(PassportHolderRecord {
                    account_id: AccountId(row.get(0)?),
                    chosen_at_unix_ms: row.get(1)?,
                    sent_version: row.get(2)?,
                    sent_at_unix_ms: row.get(3)?,
                    confirmed_version: row.get(4)?,
                    confirmed_at_unix_ms: row.get(5)?,
                })
            })
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)?;
        Ok(rows)
    }

    /// Make `account_id` a holder (nothing sent yet). Choosing a holder twice changes nothing.
    pub fn choose_passport_holder(&self, account_id: &AccountId, now_unix_ms: i64) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "
                INSERT INTO passport_holders (profile_id, account_id, chosen_at_unix_ms)
                VALUES (?1, ?2, ?3)
                ON CONFLICT(profile_id, account_id) DO NOTHING
                ",
                params![profile_id.0.as_str(), account_id.0.as_str(), now_unix_ms],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn drop_passport_holder(&self, account_id: &AccountId) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "DELETE FROM passport_holders WHERE profile_id = ?1 AND account_id = ?2",
                params![profile_id.0.as_str(), account_id.0.as_str()],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn record_passport_sent(&self, account_id: &AccountId, version: i64, now_unix_ms: i64) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "
                UPDATE passport_holders SET sent_version = ?3, sent_at_unix_ms = ?4
                WHERE profile_id = ?1 AND account_id = ?2
                ",
                params![profile_id.0.as_str(), account_id.0.as_str(), version, now_unix_ms],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// A holder confirmed `version`. An older confirmation arriving late moves nothing back.
    pub fn record_passport_confirmed(&self, account_id: &AccountId, version: i64, now_unix_ms: i64) -> OmniResult<bool> {
        let profile_id = self.require_active_profile_id()?;
        let changed = self
            .connection
            .execute(
                "
                UPDATE passport_holders SET confirmed_version = ?3, confirmed_at_unix_ms = ?4
                WHERE profile_id = ?1 AND account_id = ?2 AND confirmed_version <= ?3
                ",
                params![profile_id.0.as_str(), account_id.0.as_str(), version, now_unix_ms],
            )
            .map_err(map_sqlite_error)?;
        Ok(changed > 0)
    }

    /// Keep `owner_account_id`'s passport — the newest version only, within the quota.
    pub fn store_held_passport(&self, record: &HeldPassportRecord) -> OmniResult<HeldPassportOutcome> {
        let profile_id = self.require_active_profile_id()?;
        let existing = self.get_held_passport(&record.owner_account_id)?;
        if let Some(existing) = &existing {
            if existing.version >= record.version && existing.locator_hex == record.locator_hex {
                return Ok(HeldPassportOutcome::AlreadyNewer);
            }
        }
        let others: i64 = self
            .connection
            .query_row(
                "
                SELECT COALESCE(SUM(LENGTH(sealed)), 0) FROM held_passports
                WHERE profile_id = ?1 AND owner_account_id != ?2
                ",
                params![profile_id.0.as_str(), record.owner_account_id.0.as_str()],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        if others as usize + record.sealed.len() > HELD_PASSPORTS_MAX_TOTAL_BYTES {
            return Ok(HeldPassportOutcome::OverQuota);
        }
        self.connection
            .execute(
                "
                INSERT INTO held_passports (
                    profile_id, owner_account_id, locator_hex, version, sealed, mark_secret_hex, stored_at_unix_ms
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                ON CONFLICT(profile_id, owner_account_id) DO UPDATE SET
                    locator_hex = excluded.locator_hex,
                    version = excluded.version,
                    sealed = excluded.sealed,
                    mark_secret_hex = excluded.mark_secret_hex,
                    stored_at_unix_ms = excluded.stored_at_unix_ms
                ",
                params![
                    profile_id.0.as_str(),
                    record.owner_account_id.0.as_str(),
                    record.locator_hex,
                    record.version,
                    record.sealed,
                    record.mark_secret_hex,
                    record.stored_at_unix_ms
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(HeldPassportOutcome::Stored)
    }

    pub fn get_held_passport(&self, owner_account_id: &AccountId) -> OmniResult<Option<HeldPassportRecord>> {
        Ok(self
            .list_held_passports()?
            .into_iter()
            .find(|record| &record.owner_account_id == owner_account_id))
    }

    pub fn list_held_passports(&self) -> OmniResult<Vec<HeldPassportRecord>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT owner_account_id, locator_hex, version, sealed, mark_secret_hex, stored_at_unix_ms
                FROM held_passports WHERE profile_id = ?1 ORDER BY owner_account_id
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0.as_str()], |row| {
                Ok(HeldPassportRecord {
                    owner_account_id: AccountId(row.get(0)?),
                    locator_hex: row.get(1)?,
                    version: row.get(2)?,
                    sealed: row.get(3)?,
                    mark_secret_hex: row.get(4)?,
                    stored_at_unix_ms: row.get(5)?,
                })
            })
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)?;
        Ok(rows)
    }

    pub fn drop_held_passport(&self, owner_account_id: &AccountId) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "DELETE FROM held_passports WHERE profile_id = ?1 AND owner_account_id = ?2",
                params![profile_id.0.as_str(), owner_account_id.0.as_str()],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// Whether the chat of `dialog_key` has messages both ways — a contact we actually talk to.
    pub fn dialog_has_messages_both_ways(&self, dialog_key: &str) -> OmniResult<bool> {
        let profile_id = self.require_active_profile_id()?;
        let directions: i64 = self
            .connection
            .query_row(
                "
                SELECT COUNT(DISTINCT direction) FROM messages
                WHERE profile_id = ?1 AND dialog_key = ?2 AND direction IN ('incoming', 'outgoing')
                ",
                params![profile_id.0.as_str(), omni_types::normalize_dialog_key(dialog_key)],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        Ok(directions == 2)
    }

    /// Remember a group a restored device was in (from its passport). A group remembered twice
    /// keeps its first record.
    pub fn remember_recovered_group(&self, record: &RecoveredGroupRecord) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "
                INSERT INTO recovered_groups (profile_id, group_id, name, role, recovered_at_unix_ms)
                VALUES (?1, ?2, ?3, ?4, ?5)
                ON CONFLICT(profile_id, group_id) DO NOTHING
                ",
                params![
                    profile_id.0.as_str(),
                    record.group_id,
                    record.name,
                    record.role,
                    record.recovered_at_unix_ms
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn list_recovered_groups(&self) -> OmniResult<Vec<RecoveredGroupRecord>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT group_id, name, role, recovered_at_unix_ms FROM recovered_groups
                WHERE profile_id = ?1 ORDER BY recovered_at_unix_ms, group_id
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0.as_str()], |row| {
                Ok(RecoveredGroupRecord {
                    group_id: row.get(0)?,
                    name: row.get(1)?,
                    role: row.get(2)?,
                    recovered_at_unix_ms: row.get(3)?,
                })
            })
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)?;
        Ok(rows)
    }
}
