// Outstanding first-contact bootstrap rendezvous invites, persisted so a process restart mid-window
// doesn't silently drop them — they were in-memory only (`mesh_bootstrap_invites: Arc<Mutex<HashMap>>`),
// so a sign-in after a restart stopped the first-contact bootstrap rendezvous until the user re-paired.
// See docs/bugs/relay-less-first-contact-validate-before-advertise.md (secondary finding) +
// docs/decisions/invite-bootstrap-rendezvous.md (the persisted-republishing TODO).
//
// We store the invite NONCE, NOT the derived secret: the rendezvous bootstrap secret is recomputed from
// the nonce on load (`omni_crypto::derive_rendezvous_bootstrap_secret`), so no raw secret is written at
// rest — the nonce is the same invite material the user already shares out-of-band. `inviter_node` is the
// importer side's dial target (None on the inviter side, which doesn't yet know the importer's node).
// Pruned by absolute `expires_at_ms`. db-file scoped (per profile), like the sibling rendezvous_providers.

/// One outstanding bootstrap-invite row. The bootstrap secret is re-derived from `nonce_hex` on load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapInviteRecord {
    /// The invite's bootstrap nonce (hex). The secret is `derive_rendezvous_bootstrap_secret(nonce)`.
    pub nonce_hex: String,
    /// The 20-byte mesh node to dial (importer side); `None` on the inviter side.
    pub inviter_node: Option<[u8; 20]>,
    /// Absolute UNIX-ms deadline after which the entry is stale (min of the bootstrap window + invite TTL).
    pub expires_at_ms: i64,
}

impl Storage {
    /// Upsert an outstanding bootstrap invite — idempotent on the `nonce_hex` PK, so a re-import / refresh
    /// updates the node + expiry rather than duplicating. See `app_mesh::register_bootstrap_invite`.
    pub fn upsert_bootstrap_invite(
        &self,
        nonce_hex: &str,
        inviter_node: Option<[u8; 20]>,
        expires_at_ms: i64,
    ) -> OmniResult<()> {
        let node_blob: Option<&[u8]> = inviter_node.as_ref().map(|n| n.as_slice());
        self.connection
            .execute(
                "INSERT INTO mesh_bootstrap_invites (nonce_hex, inviter_node, expires_at_ms) \
                 VALUES (?1, ?2, ?3) \
                 ON CONFLICT(nonce_hex) DO UPDATE SET inviter_node = ?2, expires_at_ms = ?3",
                params![nonce_hex, node_blob, expires_at_ms],
            )
            .map(|_| ())
            .map_err(map_sqlite_error)
    }

    /// Every NON-EXPIRED invite (`expires_at_ms > now_ms`), to repopulate the in-memory set on sign-in.
    /// A malformed `inviter_node` blob (wrong length) decodes to `None` rather than failing the load.
    pub fn list_active_bootstrap_invites(
        &self,
        now_ms: i64,
    ) -> OmniResult<Vec<BootstrapInviteRecord>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT nonce_hex, inviter_node, expires_at_ms FROM mesh_bootstrap_invites \
                 WHERE expires_at_ms > ?1 ORDER BY expires_at_ms ASC",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![now_ms], |row| {
                let node: Option<Vec<u8>> = row.get(1)?;
                Ok(BootstrapInviteRecord {
                    nonce_hex: row.get(0)?,
                    inviter_node: node.and_then(|b| <[u8; 20]>::try_from(b.as_slice()).ok()),
                    expires_at_ms: row.get(2)?,
                })
            })
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    /// Drop one invite by nonce (the link came up, or it was cancelled). Returns rows removed.
    pub fn remove_bootstrap_invite(&self, nonce_hex: &str) -> OmniResult<usize> {
        self.connection
            .execute(
                "DELETE FROM mesh_bootstrap_invites WHERE nonce_hex = ?1",
                params![nonce_hex],
            )
            .map_err(map_sqlite_error)
    }

    /// Drop every invite whose dial target is `inviter_node` — the pairing with that node completed,
    /// so the invite is spent. Without this a restart re-armed it and paired AGAIN, re-bootstrapping a
    /// live 1:1 session (the peer kept its old one → the session diverged, docs/bugs/repair-on-restart-
    /// diverges-session.md). Returns rows removed.
    pub fn remove_bootstrap_invites_for_inviter(&self, inviter_node: &[u8; 20]) -> OmniResult<usize> {
        self.connection
            .execute(
                "DELETE FROM mesh_bootstrap_invites WHERE inviter_node = ?1",
                params![inviter_node.as_slice()],
            )
            .map_err(map_sqlite_error)
    }

    /// Sweep expired invites (`expires_at_ms <= now_ms`). Returns rows removed. Run on load + periodically.
    pub fn prune_expired_bootstrap_invites(&self, now_ms: i64) -> OmniResult<usize> {
        self.connection
            .execute(
                "DELETE FROM mesh_bootstrap_invites WHERE expires_at_ms <= ?1",
                params![now_ms],
            )
            .map_err(map_sqlite_error)
    }
}
