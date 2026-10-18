// Persisted node↔dialog bridge for mesh durable (store-and-forward) delivery.
//
// The live mesh keeps a `MeshDialogMap` (NodeAddress ↔ dialog_key) in memory,
// populated only while a Link is up. Durable delivery needs the reverse lookup
// for a peer we may NEVER have linked this session (A→C relayed via B while A
// and C are never online together): the inbound durable frame carries only the
// sender's NodeAddress, but the receive path is keyed on `dialog_key`. We record
// the binding at pairing/link-up (when both are known) so an offline-delivered
// frame can resolve its dialog_key → contact → persisted ratchet and decrypt.
//
// Correct-by-construction: we store the NodeAddress exactly as observed, rather
// than re-deriving it from a contact's public key (which depends on the mesh
// key-derivation and would be fragile). See docs/decisions/mesh-durable-delivery.md.

impl Storage {
    /// Record (or refresh) the dialog a mesh `node_address` belongs to. Idempotent;
    /// re-pairing/instance-change overwrites with the current dialog_key.
    pub fn set_mesh_dialog_route(&self, node_address: &[u8], dialog_key: &str) -> OmniResult<()> {
        self.connection
            .execute(
                "
                INSERT INTO mesh_dialog_route (node_address, dialog_key) VALUES (?1, ?2)
                ON CONFLICT(node_address) DO UPDATE SET dialog_key = excluded.dialog_key
                ",
                params![node_address, dialog_key],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// Load every persisted `(node_address, dialog_key)` route — used to repopulate
    /// the in-memory `MeshDialogMap` at mesh start so routes survive a restart.
    pub fn load_mesh_dialog_routes(&self) -> OmniResult<Vec<(Vec<u8>, String)>> {
        let mut statement = self
            .connection
            .prepare("SELECT node_address, dialog_key FROM mesh_dialog_route")
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }
}
