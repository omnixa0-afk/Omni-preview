// User-managed registry of rendezvous COURIER providers — the persistent URL set the rendezvous helper
// fans publish/fetch across (N-of-many). DISTINCT from network_nodes (mesh relay/entry points): these are
// public pub/sub brokers (ntfy HTTP, Nostr WS) used to MEET a peer, not nodes we dial for transport. The
// cross-protocol defaults are seeded once (still removable); the user adds/removes the rest from the GUI,
// and a friend's pairing invite can contribute providers (source 'invite') so a cold start on a restrictive network has a
// working shared set. Empty set falls back to the compiled default (omni_rendezvous::parse_provider_urls).
// db-file scoped (per profile), like the sibling network_nodes. See
// docs/decisions/global-identity-discovery.md (R7 invite-provider-list) + serverless-bootstrap-rendezvous.md.

/// Source tag for a seeded default provider (cross-protocol, still removable).
pub const RENDEZVOUS_PROVIDER_SOURCE_BASE: &str = "base";
/// Source tag for a provider the user added by hand from the GUI.
pub const RENDEZVOUS_PROVIDER_SOURCE_MANUAL: &str = "manual";
/// Source tag for a provider imported from a friend's mesh-pairing invite.
pub const RENDEZVOUS_PROVIDER_SOURCE_INVITE: &str = "invite";

/// We look for contacts at this courier and post our beacons to it (the default).
pub const RENDEZVOUS_PROVIDER_MODE_PUBLISH_READ: &str = "publish_read";
/// We look for contacts at this courier, but post nothing to it.
pub const RENDEZVOUS_PROVIDER_MODE_READ: &str = "read";
/// The courier is not used at all (the row stays, so a default courier can be turned back on).
pub const RENDEZVOUS_PROVIDER_MODE_OFF: &str = "off";

/// One provider row. `url` is the courier endpoint (`http(s)://` = ntfy, `ws(s)://` = a Nostr relay — the
/// scheme picks the courier at spawn). `source` is `base`/`manual`/`invite`. `id` is the removal handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RendezvousProviderRecord {
    pub id: i64,
    pub url: String,
    pub source: String,
    pub added_at_unix_ms: i64,
    /// `publish_read` / `read` / `off` (the `RENDEZVOUS_PROVIDER_MODE_*` constants).
    pub mode: String,
}

impl Storage {
    /// Add one provider URL. Idempotent on the UNIQUE `url` (`INSERT OR IGNORE`) so a re-add or an invite
    /// re-contributing a known provider is a harmless no-op. Returns rows inserted (0 = already present).
    /// Dedup/validation (scheme check) is the app layer's call.
    pub fn add_rendezvous_provider(
        &self,
        url: &str,
        source: &str,
        added_at_unix_ms: i64,
    ) -> OmniResult<usize> {
        self.connection
            .execute(
                "INSERT OR IGNORE INTO rendezvous_providers (url, source, added_at_unix_ms) \
                 VALUES (?1, ?2, ?3)",
                params![url, source, added_at_unix_ms],
            )
            .map_err(map_sqlite_error)
    }

    /// Every provider, seeded defaults first then oldest-added — the order the GUI shows and the helper
    /// fans out across.
    pub fn list_rendezvous_providers(&self) -> OmniResult<Vec<RendezvousProviderRecord>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, url, source, added_at_unix_ms, mode FROM rendezvous_providers \
                 ORDER BY (source = 'base') DESC, added_at_unix_ms ASC, id ASC",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok(RendezvousProviderRecord {
                    id: row.get(0)?,
                    url: row.get(1)?,
                    source: row.get(2)?,
                    added_at_unix_ms: row.get(3)?,
                    mode: row.get(4)?,
                })
            })
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    /// Set how a provider is used (`RENDEZVOUS_PROVIDER_MODE_*`). Returns rows changed (0 = no such id).
    pub fn set_rendezvous_provider_mode(&self, id: i64, mode: &str) -> OmniResult<usize> {
        if ![RENDEZVOUS_PROVIDER_MODE_PUBLISH_READ, RENDEZVOUS_PROVIDER_MODE_READ, RENDEZVOUS_PROVIDER_MODE_OFF]
            .contains(&mode)
        {
            return Err(OmniError::InvalidInput(format!("courier mode '{mode}' is unknown")));
        }
        self.connection
            .execute(
                "UPDATE rendezvous_providers SET mode = ?2 WHERE id = ?1",
                params![id, mode],
            )
            .map_err(map_sqlite_error)
    }

    /// Remove ANY provider by id — including a seeded default (the user may delete it; an empty set falls
    /// back to the compiled cross-protocol default). Returns rows removed (0 = no such id).
    pub fn remove_rendezvous_provider(&self, id: i64) -> OmniResult<usize> {
        self.connection
            .execute(
                "DELETE FROM rendezvous_providers WHERE id = ?1",
                params![id],
            )
            .map_err(map_sqlite_error)
    }

    /// True if a provider with this exact URL already exists — lets the app layer reject a duplicate add
    /// cheaply (and tell the GUI).
    pub fn rendezvous_provider_exists(&self, url: &str) -> OmniResult<bool> {
        let count: i64 = self
            .connection
            .query_row(
                "SELECT COUNT(*) FROM rendezvous_providers WHERE url = ?1",
                params![url],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        Ok(count > 0)
    }
}
