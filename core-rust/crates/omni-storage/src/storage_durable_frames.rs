// Durable-delivery store-and-forward frame persistence (mesh P4).
//
// omni-storage holds these as **opaque wire bytes** keyed by `(feed_id, seq)` —
// it has no dependency on the omni-network frame format (clean layering; the
// network crate stays storage-agnostic). The mesh runtime snapshots the frames a
// node holds for others via `Network::export_durable`, persists them here, and
// restores them through `Network::import_durable` on startup so store-and-forward
// survives a restart. Node-global (not profile-scoped): these are relay payload
// for arbitrary destinations, not the user's content.
//
// See docs/decisions/mesh-durable-delivery.md.

impl Storage {
    /// Replace all persisted durable frames with `frames` — a list of
    /// `(feed_id, seq, wire)`. Matches the network layer's snapshot seam
    /// (`export_durable` → replace). Clear-then-insert so deleted/expired/
    /// delivered frames don't linger across restart. Returns the count stored.
    pub fn replace_durable_frames(&self, frames: &[(Vec<u8>, i64, Vec<u8>)]) -> OmniResult<usize> {
        // One transaction: this now runs periodically (not only on mesh stop), and thousands of
        // autocommitted inserts would each pay an fsync.
        self.with_immediate_transaction("replace_durable_frames", |storage| {
            storage
                .connection
                .execute("DELETE FROM durable_frames", [])
                .map_err(map_sqlite_error)?;
            for (feed_id, seq, wire) in frames {
                storage
                    .connection
                    .execute(
                        "INSERT INTO durable_frames (feed_id, seq, wire) VALUES (?1, ?2, ?3)",
                        params![feed_id, seq, wire],
                    )
                    .map_err(map_sqlite_error)?;
            }
            Ok(frames.len())
        })
    }

    /// Atomically increment and return the next durable feed sequence for `peer`
    /// (per-recipient monotone). Persisted on EVERY call so it survives restart —
    /// a reused seq would let the recipient false-dedup a post-restart message
    /// (the held frame lingers until TTL). Contiguous, so the anti-entropy
    /// high-water stays tight. See docs/notes/durable-1to1-integration-plan.md.
    pub fn next_durable_seq(&self, peer: &[u8]) -> OmniResult<i64> {
        self.connection
            .execute(
                "
                INSERT INTO durable_feed_seq (peer, seq) VALUES (?1, 1)
                ON CONFLICT(peer) DO UPDATE SET seq = seq + 1
                ",
                params![peer],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .query_row(
                "SELECT seq FROM durable_feed_seq WHERE peer = ?1",
                params![peer],
                |row| row.get::<_, i64>(0),
            )
            .map_err(map_sqlite_error)
    }

    /// Load all persisted durable-frame wire-blobs (to repopulate the in-memory
    /// store at startup via `Network::import_durable`).
    pub fn load_durable_frames(&self) -> OmniResult<Vec<Vec<u8>>> {
        let mut statement = self
            .connection
            .prepare("SELECT wire FROM durable_frames ORDER BY feed_id, seq")
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }
}
