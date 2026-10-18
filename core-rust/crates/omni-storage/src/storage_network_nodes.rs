// User-managed registry of network ENTRY nodes (bootstrap / relay points) — the persistent list the
// mesh dials when LAN announce + invite don't reach (rendezvous / relay / bootstrap). DISTINCT from
// mesh_peer_book (auto-cached DIRECT endpoints of CONTACTS for warm rejoin): these are operator-curated
// entry points, not peers. Exactly ONE removable "base" node is seeded once by the app layer (the
// project VPS); the user adds/removes the rest from the GUI. The empty list MUST still connect (LAN +
// invite) — no node is privileged (threat-model). Endpoints are HINTS: the
// ed25519 challenge on every dial is the sole authority, so a stale/hostile entry can at worst waste a
// dial. db-file scoped (per profile), like the sibling mesh_peer_book. See
// docs/decisions/community-volunteer-nodes.md + docs/notes/roadmap-CONTINUE.md (Always-connect epic).

/// Source tag for the one seeded, still-removable base bootstrap node (the project VPS).
pub const NETWORK_NODE_SOURCE_BASE: &str = "base";
/// Source tag for a node the user added by hand (ip:port or token/address).
pub const NETWORK_NODE_SOURCE_MANUAL: &str = "manual";

/// One entry-node row. `endpoint` (ip:port) and/or `node_address` (mesh identity) say where/whom to dial
/// — at least one is present. `source` is `base` (the seeded removable base node) or `manual` (user-added).
/// `id` is the stable handle for removal.
///
/// `endpoint` is the UDP/QUIC hint (rung 1 — relay over native UDP). `tcp_endpoint` is the SEPARATE
/// TLS-over-TCP bridge address (rung 0 — the rock-solid carrier that survives a UDP-hostile carrier
/// like a UDP-dropping carrier); `cert_spki_pin` is the optional SPKI hash that pins the outer TLS for the R2
/// active-probe-grade fronting. Both are additive and nullable: a node may offer only UDP, only TCP, or
/// both. See docs/decisions/connection-quality-ladder.md (the rung-0..2 ladder).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkNodeRecord {
    pub id: i64,
    pub endpoint: Option<String>,
    pub node_address: Option<Vec<u8>>,
    pub label: String,
    pub source: String,
    pub added_at_unix_ms: i64,
    pub tcp_endpoint: Option<String>,
    pub cert_spki_pin: Option<Vec<u8>>,
    /// R1c: the issuing identity's relay-bundle id this row belongs to (NULL for a manually-added
    /// node). Rows sharing a `bundle_id` are replaced wholesale when a newer `bundle_version` arrives.
    pub bundle_id: Option<String>,
    /// R1c: the bundle version this row was imported at (NULL for a manual node). Monotone per
    /// `bundle_id`; a higher version supersedes lower ones. See `max_bundle_version` /
    /// `remove_network_nodes_by_bundle`. docs/decisions/r1c-rotatable-white-relay.md.
    pub bundle_version: Option<i64>,
    /// R1c increment 3b: this relay is flagged as part of MY OWN relay bundle — the set I publish to
    /// approved friends over the per-pair beacon (issuer side). Orthogonal to `bundle_id` (which tags
    /// relays IMPORTED from a friend). Set via `set_network_node_shared`. docs/decisions/r1c-rotatable-white-relay.md.
    pub shared: bool,
}

/// Fields for inserting one entry node. A struct (not a long positional argument list) so the
/// registry's growing column set stays readable at call sites — borrows, so callers build it inline
/// with no allocation. `bundle_id`/`bundle_version` tag a relay imported from a signed `om3relay:`
/// bundle (NULL for a manual add). See docs/decisions/r1c-rotatable-white-relay.md.
#[derive(Debug, Default, Clone, Copy)]
pub struct NewNetworkNode<'a> {
    pub endpoint: Option<&'a str>,
    pub node_address: Option<&'a [u8]>,
    pub label: &'a str,
    pub source: &'a str,
    pub added_at_unix_ms: i64,
    pub tcp_endpoint: Option<&'a str>,
    pub cert_spki_pin: Option<&'a [u8]>,
    pub bundle_id: Option<&'a str>,
    pub bundle_version: Option<i64>,
}

impl Storage {
    /// Insert one entry node, returning its new id. Thin store primitive — at least one of
    /// `endpoint`/`node_address` should be present; dedup is the app layer's call (see
    /// `network_node_endpoint_exists`), so the GUI can reject re-adding the same endpoint.
    pub fn add_network_node(&self, node: &NewNetworkNode<'_>) -> OmniResult<i64> {
        self.connection
            .execute(
                "INSERT INTO network_nodes \
                 (endpoint, node_address, label, source, added_at_unix_ms, tcp_endpoint, cert_spki_pin, \
                  bundle_id, bundle_version) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    node.endpoint,
                    node.node_address,
                    node.label,
                    node.source,
                    node.added_at_unix_ms,
                    node.tcp_endpoint,
                    node.cert_spki_pin,
                    node.bundle_id,
                    node.bundle_version
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(self.connection.last_insert_rowid())
    }

    /// Every entry node, base first then oldest-added — the order the GUI shows and the mesh dials.
    pub fn list_network_nodes(&self) -> OmniResult<Vec<NetworkNodeRecord>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, endpoint, node_address, label, source, added_at_unix_ms, \
                        tcp_endpoint, cert_spki_pin, bundle_id, bundle_version, shared \
                 FROM network_nodes \
                 ORDER BY (source = 'base') DESC, added_at_unix_ms ASC, id ASC",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok(NetworkNodeRecord {
                    id: row.get(0)?,
                    endpoint: row.get(1)?,
                    node_address: row.get(2)?,
                    label: row.get(3)?,
                    source: row.get(4)?,
                    added_at_unix_ms: row.get(5)?,
                    tcp_endpoint: row.get(6)?,
                    cert_spki_pin: row.get(7)?,
                    bundle_id: row.get(8)?,
                    bundle_version: row.get(9)?,
                    shared: row.get::<_, i64>(10)? != 0,
                })
            })
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    /// Remove ANY entry node by id — INCLUDING the base node (the user may delete it; the empty list
    /// still connects via LAN + invite). Returns rows removed (0 = no such id).
    pub fn remove_network_node(&self, id: i64) -> OmniResult<usize> {
        self.connection
            .execute("DELETE FROM network_nodes WHERE id = ?1", params![id])
            .map_err(map_sqlite_error)
    }

    /// True if an entry node with this exact endpoint already exists — lets the app layer reject a
    /// duplicate add cheaply.
    pub fn network_node_endpoint_exists(&self, endpoint: &str) -> OmniResult<bool> {
        let count: i64 = self
            .connection
            .query_row(
                "SELECT COUNT(*) FROM network_nodes WHERE endpoint = ?1",
                params![endpoint],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        Ok(count > 0)
    }

    /// Highest `bundle_version` currently stored for `bundle_id`, or `None` if no row carries it.
    /// R1c rotation guard: an incoming `om3relay:` bundle is only applied if its version is strictly
    /// greater than this — a replay/older bundle is ignored, so an attacker can't roll a friend back to a
    /// burned relay set.
    pub fn max_bundle_version(&self, bundle_id: &str) -> OmniResult<Option<i64>> {
        // MAX over an all-NULL / empty set yields a single NULL row, so query for Option<i64>.
        self.connection
            .query_row(
                "SELECT MAX(bundle_version) FROM network_nodes WHERE bundle_id = ?1",
                params![bundle_id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(map_sqlite_error)
    }

    /// Delete every row tagged with `bundle_id`, returning the count removed. R1c rotation step: before
    /// inserting a newer bundle version, wipe the issuer's prior relay set so the new set REPLACES it
    /// (no stale accumulation). Manual nodes (NULL `bundle_id`) are never touched.
    pub fn remove_network_nodes_by_bundle(&self, bundle_id: &str) -> OmniResult<usize> {
        self.connection
            .execute(
                "DELETE FROM network_nodes WHERE bundle_id = ?1",
                params![bundle_id],
            )
            .map_err(map_sqlite_error)
    }

    /// R1c increment 3b: flag (or unflag) entry node `id` as part of MY OWN relay bundle — the set I
    /// publish to approved friends over the per-pair beacon. Returns rows updated (0 = no such id). The
    /// app layer bumps my bundle version after toggling so a rotated set propagates. Idempotent.
    pub fn set_network_node_shared(&self, id: i64, shared: bool) -> OmniResult<usize> {
        self.connection
            .execute(
                "UPDATE network_nodes SET shared = ?2 WHERE id = ?1",
                params![id, i64::from(shared)],
            )
            .map_err(map_sqlite_error)
    }

    /// R1c increment 3b: the relays I currently publish as MY OWN bundle (flagged `shared`), in the
    /// same base-first / oldest-added order as `list_network_nodes`. Empty ⇒ I publish no bundle.
    pub fn list_shared_network_nodes(&self) -> OmniResult<Vec<NetworkNodeRecord>> {
        Ok(self
            .list_network_nodes()?
            .into_iter()
            .filter(|node| node.shared)
            .collect())
    }

    /// Drop the whole registry (profile delete). NOT called on sign-out — the relay list is the user's
    /// durable setting and must outlive a session.
    pub fn clear_network_nodes(&self) -> OmniResult<()> {
        self.connection
            .execute("DELETE FROM network_nodes", [])
            .map_err(map_sqlite_error)?;
        Ok(())
    }
}
