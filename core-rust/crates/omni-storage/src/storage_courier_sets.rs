// Where the devices we talk to post their beacons and where they look for others'
// (docs/decisions/courier-metadata-and-quota.md, step 6). Each device of a contact — and each other
// device of ours — tells us its courier set; we post a pair's beacon where that device looks and look
// where it posts. One row per (account, device); the latest set it told us replaces the one before.

/// A device's courier set, by URL: `publish` — where it posts its beacons; `read` — where it looks for
/// others' (every courier it posts to, and the ones it only reads).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PeerCourierSet {
    pub publish: Vec<String>,
    pub read: Vec<String>,
}

impl Storage {
    /// Remember what `device_id` of `account_id` told us. Returns whether it changed anything.
    pub fn record_peer_courier_set(
        &self,
        account_id: &AccountId,
        device_id: &DeviceId,
        set: &PeerCourierSet,
        now_unix_ms: i64,
    ) -> OmniResult<bool> {
        let profile_id = self.require_active_profile_id()?;
        if self.get_peer_courier_set(account_id, device_id)?.as_ref() == Some(set) {
            return Ok(false);
        }
        let publish = serde_json::to_string(&set.publish).map_err(|error| OmniError::Storage(error.to_string()))?;
        let read = serde_json::to_string(&set.read).map_err(|error| OmniError::Storage(error.to_string()))?;
        self.connection
            .execute(
                "
                INSERT INTO peer_courier_sets (profile_id, remote_account_id, remote_device_id, publish_json, read_json, updated_at_unix_ms)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ON CONFLICT(profile_id, remote_account_id, remote_device_id) DO UPDATE SET
                    publish_json = excluded.publish_json,
                    read_json = excluded.read_json,
                    updated_at_unix_ms = excluded.updated_at_unix_ms
                ",
                params![profile_id.0.as_str(), account_id.0.as_str(), device_id.0.as_str(), publish, read, now_unix_ms],
            )
            .map_err(map_sqlite_error)?;
        Ok(true)
    }

    /// What `device_id` of `account_id` last told us, if it ever did.
    pub fn get_peer_courier_set(&self, account_id: &AccountId, device_id: &DeviceId) -> OmniResult<Option<PeerCourierSet>> {
        let profile_id = self.require_active_profile_id()?;
        let row: Option<(String, String)> = self
            .connection
            .query_row(
                "
                SELECT publish_json, read_json FROM peer_courier_sets
                WHERE profile_id = ?1 AND remote_account_id = ?2 AND remote_device_id = ?3
                ",
                params![profile_id.0.as_str(), account_id.0.as_str(), device_id.0.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        row.map(|(publish, read)| {
            Ok(PeerCourierSet {
                publish: serde_json::from_str(&publish).map_err(|error| OmniError::Storage(error.to_string()))?,
                read: serde_json::from_str(&read).map_err(|error| OmniError::Storage(error.to_string()))?,
            })
        })
        .transpose()
    }

    /// Every courier URL any device we talk to told us about, without repeats, sorted.
    pub fn list_peer_courier_urls(&self) -> OmniResult<Vec<String>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare("SELECT publish_json, read_json FROM peer_courier_sets WHERE profile_id = ?1")
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0.as_str()], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)?;
        let mut urls: Vec<String> = Vec::new();
        for (publish, read) in rows {
            for json in [publish, read] {
                for url in serde_json::from_str::<Vec<String>>(&json).unwrap_or_default() {
                    if !urls.contains(&url) {
                        urls.push(url);
                    }
                }
            }
        }
        urls.sort();
        Ok(urls)
    }
}
