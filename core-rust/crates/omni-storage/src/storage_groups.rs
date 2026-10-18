// Groups — what the app keeps around MLS (docs/decisions/group-crypto-mls.md).
//
// The source of truth for membership and keys is the MLS state (`omni-group-mls`, OpenMLS tables in
// this same database). What lives here is the app's side: the group row (name, our status, the
// thread key, our durable-feed sequence), a PROJECTION of the members refreshed from MLS after every
// Commit (calls and the UI read it), pending invitations, MLS messages from an epoch we have not
// reached yet, our own messages/changes per epoch (re-sent if a concurrent Commit rolls their epoch
// back), and who wrote each group message. Profile-scoped.

/// A group as the profile sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupRecord {
    pub group_id: String,
    pub name: String,
    /// Our account signing pub (base64) — which member we are.
    pub my_member_pub: String,
    /// member | left | removed.
    pub my_status: String,
    /// Chat-thread key: `group:<group_id>`.
    pub dialog_key: String,
    pub created_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
}

/// One member, projected from the MLS group after each Commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMemberRecord {
    pub member_pub: String,
    pub account_id: String,
    pub display_name: String,
    /// admin | member.
    pub role: String,
    /// member | left (said "leaving", not yet removed by an admin).
    pub status: String,
    /// Mesh node of the member (derived from `member_pub`), hex — calls and the subscription use it.
    pub node_hint_hex: String,
}

/// An invitation we received over a 1:1 channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupInviteRecord {
    pub group_id: String,
    pub name: String,
    pub inviter_name: String,
    pub inviter_dialog_key: String,
    /// pending | accepted | declined.
    pub status: String,
    pub received_at_unix_ms: i64,
}

/// Who wrote a message of a group thread, and whether it is a system line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMessageMeta {
    pub message_id: String,
    pub sender_pub: Option<String>,
    pub sender_name: String,
    /// text | system.
    pub kind: String,
}

pub fn group_dialog_key(group_id: &str) -> String {
    format!("group:{group_id}")
}

impl Storage {
    pub fn upsert_group(&self, record: &GroupRecord) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "
                INSERT INTO groups (
                    profile_id, group_id, name, my_member_pub, my_status, dialog_key,
                    created_at_unix_ms, updated_at_unix_ms
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                ON CONFLICT(profile_id, group_id) DO UPDATE SET
                    name = excluded.name,
                    my_member_pub = excluded.my_member_pub,
                    my_status = excluded.my_status,
                    dialog_key = excluded.dialog_key,
                    updated_at_unix_ms = excluded.updated_at_unix_ms
                ",
                params![
                    profile_id.0,
                    record.group_id,
                    record.name,
                    record.my_member_pub,
                    record.my_status,
                    record.dialog_key,
                    record.created_at_unix_ms,
                    record.updated_at_unix_ms,
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn get_group(&self, group_id: &str) -> OmniResult<Option<GroupRecord>> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .query_row(
                "SELECT group_id, name, my_member_pub, my_status, dialog_key, created_at_unix_ms, updated_at_unix_ms
                 FROM groups WHERE profile_id = ?1 AND group_id = ?2",
                params![profile_id.0, group_id],
                map_group_record_row,
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    pub fn get_group_by_dialog_key(&self, dialog_key: &str) -> OmniResult<Option<GroupRecord>> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .query_row(
                "SELECT group_id, name, my_member_pub, my_status, dialog_key, created_at_unix_ms, updated_at_unix_ms
                 FROM groups WHERE profile_id = ?1 AND dialog_key = ?2",
                params![profile_id.0, dialog_key],
                map_group_record_row,
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    pub fn list_groups(&self) -> OmniResult<Vec<GroupRecord>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT group_id, name, my_member_pub, my_status, dialog_key, created_at_unix_ms, updated_at_unix_ms
                 FROM groups WHERE profile_id = ?1 ORDER BY updated_at_unix_ms DESC",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0], map_group_record_row)
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    /// Forget a group's app-side rows (the MLS state is the engine's to delete; the chat lines are
    /// [`Storage::delete_dialog_history`]'s).
    pub fn delete_group_rows(&self, group_id: &str) -> OmniResult<()> {
        self.delete_group_tables(group_id, &["groups", "group_members", "group_feed_outbox"])?;
        self.delete_group_chat_rows(group_id)
    }

    /// Everything of the group but what its last queued feed messages still need to go out (the
    /// group row for our feed and its seq, the members for the subscription, the queue itself):
    /// a farewell queued while offline is published later, then [`Storage::delete_group_rows`].
    pub fn delete_group_chat_rows(&self, group_id: &str) -> OmniResult<()> {
        self.delete_group_tables(
            group_id,
            &[
                "group_pending_inbound",
                "group_outbox",
                "group_own_changes",
                "group_sent_invites",
                "group_message_meta",
                "group_invites",
            ],
        )
    }

    /// Groups this profile has a chat for but no row: the candidates to restore from their MLS state
    /// (an old build's migration dropped the rows, docs/bugs/old-build-wiped-groups.md). A group left
    /// or deleted here took its chat with it, so it is no candidate.
    pub fn list_group_ids_without_row(&self) -> OmniResult<Vec<String>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT DISTINCT substr(dialog_key, 7) FROM messages
                 WHERE profile_id = ?1 AND dialog_key LIKE 'group:%'
                   AND substr(dialog_key, 7) NOT IN (SELECT group_id FROM groups WHERE profile_id = ?1)",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0], |row| row.get::<_, String>(0))
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    /// How far OUR feed of the group may already have gone: the highest seq of our frames still held
    /// for it, and the number of things we put on it (app messages + commits). A restored row starts
    /// past both — a reused seq is dropped by every member as already seen.
    pub fn group_feed_seq_floor(&self, group_id: &str, feed_id: &[u8]) -> OmniResult<u64> {
        let profile_id = self.require_active_profile_id()?;
        let held: i64 = self
            .connection
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM durable_frames WHERE feed_id = ?1",
                params![feed_id],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        let put: i64 = self
            .connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM group_outbox WHERE profile_id = ?1 AND group_id = ?2)
                      + (SELECT COUNT(*) FROM group_own_changes WHERE profile_id = ?1 AND group_id = ?2)",
                params![profile_id.0, group_id],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        Ok(held.max(put).max(0) as u64)
    }

    pub fn set_group_feed_seq(&self, group_id: &str, seq: u64) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "UPDATE groups SET feed_seq = ?3 WHERE profile_id = ?1 AND group_id = ?2",
                params![profile_id.0, group_id, seq as i64],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn count_group_feed_outbox(&self, group_id: &str) -> OmniResult<usize> {
        let profile_id = self.require_active_profile_id()?;
        let count: i64 = self
            .connection
            .query_row(
                "SELECT COUNT(*) FROM group_feed_outbox WHERE profile_id = ?1 AND group_id = ?2",
                params![profile_id.0, group_id],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        Ok(count as usize)
    }

    fn delete_group_tables(&self, group_id: &str, tables: &[&str]) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        for table in tables {
            self.connection
                .execute(
                    &format!("DELETE FROM {table} WHERE profile_id = ?1 AND group_id = ?2"),
                    params![profile_id.0, group_id],
                )
                .map_err(map_sqlite_error)?;
        }
        Ok(())
    }

    /// Next sequence number of OUR durable feed in this group (1-based, monotone).
    pub fn next_group_feed_seq(&self, group_id: &str) -> OmniResult<u64> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "UPDATE groups SET feed_seq = feed_seq + 1 WHERE profile_id = ?1 AND group_id = ?2",
                params![profile_id.0, group_id],
            )
            .map_err(map_sqlite_error)?;
        let seq: i64 = self
            .connection
            .query_row(
                "SELECT feed_seq FROM groups WHERE profile_id = ?1 AND group_id = ?2",
                params![profile_id.0, group_id],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;
        Ok(seq as u64)
    }

    /// Replace the member projection (after a Commit / a join). Keeps a member's `left` mark while
    /// they are still in the MLS group.
    pub fn replace_group_members(&self, group_id: &str, members: &[GroupMemberRecord]) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        let left: Vec<String> = self
            .list_group_members(group_id)?
            .into_iter()
            .filter(|m| m.status == "left")
            .map(|m| m.member_pub)
            .collect();
        self.connection
            .execute(
                "DELETE FROM group_members WHERE profile_id = ?1 AND group_id = ?2",
                params![profile_id.0, group_id],
            )
            .map_err(map_sqlite_error)?;
        for m in members {
            let status = if left.contains(&m.member_pub) { "left" } else { m.status.as_str() };
            self.connection
                .execute(
                    "INSERT INTO group_members (
                        profile_id, group_id, member_pub, account_id, display_name, role, status, node_hint_hex
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        profile_id.0,
                        group_id,
                        m.member_pub,
                        m.account_id,
                        m.display_name,
                        m.role,
                        status,
                        m.node_hint_hex
                    ],
                )
                .map_err(map_sqlite_error)?;
        }
        Ok(())
    }

    pub fn set_group_member_status(&self, group_id: &str, member_pub: &str, status: &str) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "UPDATE group_members SET status = ?4 WHERE profile_id = ?1 AND group_id = ?2 AND member_pub = ?3",
                params![profile_id.0, group_id, member_pub, status],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn list_group_members(&self, group_id: &str) -> OmniResult<Vec<GroupMemberRecord>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT member_pub, account_id, display_name, role, status, node_hint_hex
                 FROM group_members WHERE profile_id = ?1 AND group_id = ?2 ORDER BY display_name",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0, group_id], |row| {
                Ok(GroupMemberRecord {
                    member_pub: row.get(0)?,
                    account_id: row.get(1)?,
                    display_name: row.get(2)?,
                    role: row.get(3)?,
                    status: row.get(4)?,
                    node_hint_hex: row.get(5)?,
                })
            })
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    // --- invitations ---------------------------------------------------------------------------

    pub fn upsert_group_invite(&self, invite: &GroupInviteRecord) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "INSERT INTO group_invites (
                    profile_id, group_id, name, inviter_name, inviter_dialog_key, status, received_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(profile_id, group_id) DO UPDATE SET
                    name = excluded.name,
                    inviter_name = excluded.inviter_name,
                    inviter_dialog_key = excluded.inviter_dialog_key,
                    status = excluded.status,
                    received_at_unix_ms = excluded.received_at_unix_ms",
                params![
                    profile_id.0,
                    invite.group_id,
                    invite.name,
                    invite.inviter_name,
                    invite.inviter_dialog_key,
                    invite.status,
                    invite.received_at_unix_ms
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn get_group_invite(&self, group_id: &str) -> OmniResult<Option<GroupInviteRecord>> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .query_row(
                "SELECT group_id, name, inviter_name, inviter_dialog_key, status, received_at_unix_ms
                 FROM group_invites WHERE profile_id = ?1 AND group_id = ?2",
                params![profile_id.0, group_id],
                map_group_invite_row,
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    /// Invitations still in play: unanswered ("pending") and accepted-but-not-yet-joined
    /// ("accepted" — the Welcome is on its way; the app hides those whose group already exists).
    pub fn list_open_group_invites(&self) -> OmniResult<Vec<GroupInviteRecord>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT group_id, name, inviter_name, inviter_dialog_key, status, received_at_unix_ms
                 FROM group_invites WHERE profile_id = ?1 AND status IN ('pending', 'accepted')
                 ORDER BY received_at_unix_ms DESC",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0], map_group_invite_row)
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    // --- MLS messages for an epoch we have not reached -----------------------------------------

    pub fn push_group_pending_inbound(&self, group_id: &str, epoch: u64, mls: &[u8], now: i64) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "INSERT OR IGNORE INTO group_pending_inbound (profile_id, group_id, epoch, mls, received_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![profile_id.0, group_id, epoch as i64, mls, now],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// Take (and delete) the pending messages of a group, oldest epoch first.
    pub fn take_group_pending_inbound(&self, group_id: &str) -> OmniResult<Vec<Vec<u8>>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT mls FROM group_pending_inbound WHERE profile_id = ?1 AND group_id = ?2
                 ORDER BY epoch, received_at_unix_ms",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0, group_id], |row| row.get::<_, Vec<u8>>(0))
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "DELETE FROM group_pending_inbound WHERE profile_id = ?1 AND group_id = ?2",
                params![profile_id.0, group_id],
            )
            .map_err(map_sqlite_error)?;
        Ok(rows)
    }

    // --- our own messages / changes per epoch (re-sent after a rollback) ----------------------

    pub fn record_group_outbox(&self, group_id: &str, epoch: u64, message_id: &str, payload_json: &str, now: i64) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "INSERT OR REPLACE INTO group_outbox (profile_id, group_id, epoch, message_id, payload_json, sent_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![profile_id.0, group_id, epoch as i64, message_id, payload_json, now],
            )
            .map_err(map_sqlite_error)?;
        // Only the last few epochs can be rolled back; older entries are dead weight.
        self.connection
            .execute(
                "DELETE FROM group_outbox WHERE profile_id = ?1 AND group_id = ?2 AND epoch < ?3",
                params![profile_id.0, group_id, epoch as i64 - 2],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// Our messages sent at `epoch` or later (payload JSON), oldest first.
    pub fn list_group_outbox_since(&self, group_id: &str, epoch: u64) -> OmniResult<Vec<String>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT payload_json FROM group_outbox WHERE profile_id = ?1 AND group_id = ?2 AND epoch >= ?3
                 ORDER BY sent_at_unix_ms",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0, group_id, epoch as i64], |row| row.get::<_, String>(0))
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    /// What our own Commit for `epoch_after` did (JSON), so a lost Commit can be re-issued.
    pub fn record_group_own_change(&self, group_id: &str, epoch_after: u64, change_json: &str) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "INSERT OR REPLACE INTO group_own_changes (profile_id, group_id, epoch_after, change_json)
                 VALUES (?1, ?2, ?3, ?4)",
                params![profile_id.0, group_id, epoch_after as i64, change_json],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "DELETE FROM group_own_changes WHERE profile_id = ?1 AND group_id = ?2 AND epoch_after < ?3",
                params![profile_id.0, group_id, epoch_after as i64 - GROUP_OWN_CHANGES_KEPT_EPOCHS],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn take_group_own_change(&self, group_id: &str, epoch_after: u64) -> OmniResult<Option<String>> {
        let profile_id = self.require_active_profile_id()?;
        let change: Option<String> = self
            .connection
            .query_row(
                "SELECT change_json FROM group_own_changes WHERE profile_id = ?1 AND group_id = ?2 AND epoch_after = ?3",
                params![profile_id.0, group_id, epoch_after as i64],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "DELETE FROM group_own_changes WHERE profile_id = ?1 AND group_id = ?2 AND epoch_after = ?3",
                params![profile_id.0, group_id, epoch_after as i64],
            )
            .map_err(map_sqlite_error)?;
        Ok(change)
    }

    // --- who wrote what -----------------------------------------------------------------------

    pub fn insert_group_message_meta(&self, group_id: &str, meta: &GroupMessageMeta) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "INSERT OR IGNORE INTO group_message_meta (profile_id, group_id, message_id, sender_pub, sender_name, kind)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![profile_id.0, group_id, meta.message_id, meta.sender_pub, meta.sender_name, meta.kind],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// The author/kind of one line (the chat-list preview needs only the latest one).
    pub fn get_group_message_meta(&self, group_id: &str, message_id: &str) -> OmniResult<Option<GroupMessageMeta>> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .query_row(
                "SELECT message_id, sender_pub, sender_name, kind FROM group_message_meta
                 WHERE profile_id = ?1 AND group_id = ?2 AND message_id = ?3",
                params![profile_id.0, group_id, message_id],
                |row| {
                    Ok(GroupMessageMeta {
                        message_id: row.get(0)?,
                        sender_pub: row.get(1)?,
                        sender_name: row.get(2)?,
                        kind: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    pub fn list_group_message_meta(&self, group_id: &str) -> OmniResult<Vec<GroupMessageMeta>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT message_id, sender_pub, sender_name, kind FROM group_message_meta
                 WHERE profile_id = ?1 AND group_id = ?2",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0, group_id], |row| {
                Ok(GroupMessageMeta {
                    message_id: row.get(0)?,
                    sender_pub: row.get(1)?,
                    sender_name: row.get(2)?,
                    kind: row.get(3)?,
                })
            })
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    // --- what we publish on the group feed (flushed by whoever holds the mesh) ----------------

    /// Queue one MLS message for our durable feed in `group_id`. The single publish path: a method
    /// of the app flushes at once when the mesh is up, the mesh worker flushes on its tick — and a
    /// message written while the mesh is down simply waits here.
    pub fn enqueue_group_feed(&self, group_id: &str, mls: &[u8], now: i64) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "INSERT INTO group_feed_outbox (profile_id, group_id, mls, created_at_unix_ms) VALUES (?1, ?2, ?3, ?4)",
                params![profile_id.0, group_id, mls, now],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// The queued feed messages, oldest first: (row id, group id, MLS bytes).
    pub fn list_group_feed_outbox(&self, limit: usize) -> OmniResult<Vec<(i64, String, Vec<u8>)>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, group_id, mls FROM group_feed_outbox WHERE profile_id = ?1 ORDER BY id LIMIT ?2",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0, limit as i64], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    pub fn delete_group_feed_outbox(&self, id: i64) -> OmniResult<()> {
        self.connection
            .execute("DELETE FROM group_feed_outbox WHERE id = ?1", params![id])
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    // --- invitations we sent (an accept is honoured only from someone we invited) --------------

    pub fn record_group_sent_invite(&self, group_id: &str, dialog_key: &str, now: i64) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "INSERT OR REPLACE INTO group_sent_invites (profile_id, group_id, dialog_key, sent_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4)",
                params![profile_id.0, group_id, dialog_key, now],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// Invitations we sent for `group_id` that nobody has answered yet: (dialog_key, sent_at).
    pub fn list_group_sent_invites(&self, group_id: &str) -> OmniResult<Vec<(String, i64)>> {
        let profile_id = self.require_active_profile_id()?;
        let mut stmt = self
            .connection
            .prepare(
                "SELECT dialog_key, sent_at_unix_ms FROM group_sent_invites
                 WHERE profile_id = ?1 AND group_id = ?2 ORDER BY sent_at_unix_ms",
            )
            .map_err(map_sqlite_error)?;
        let rows = stmt
            .query_map(params![profile_id.0, group_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    /// Consume the invitation we sent to `dialog_key` for `group_id`: `true` if there was one.
    pub fn take_group_sent_invite(&self, group_id: &str, dialog_key: &str) -> OmniResult<bool> {
        let profile_id = self.require_active_profile_id()?;
        let removed = self
            .connection
            .execute(
                "DELETE FROM group_sent_invites WHERE profile_id = ?1 AND group_id = ?2 AND dialog_key = ?3",
                params![profile_id.0, group_id, dialog_key],
            )
            .map_err(map_sqlite_error)?;
        Ok(removed > 0)
    }

    // --- the key that encrypts MLS state at rest ------------------------------------------------

    /// Wipe all MLS state of this database (OpenMLS's own tables, our epoch snapshots, the wrapped
    /// at-rest key). Run when the LAST profile is deleted: the MLS rows are not profile-scoped, and
    /// leaving them would keep a deleted account's group secrets on disk. Tables that were never
    /// created (no group was ever touched) are simply absent. Returns rows removed.
    pub fn wipe_mls_state(&self) -> OmniResult<usize> {
        let tables: Vec<String> = self
            .connection
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' \
                 AND (name LIKE 'openmls\\_%' ESCAPE '\\' OR name IN ('omni_mls_snapshots', 'mls_storage_key')) \
                 AND name <> 'openmls_sqlite_storage_migrations'",
            )
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()
            })
            .map_err(map_sqlite_error)?;
        let mut removed = 0;
        for table in tables {
            removed += self
                .connection
                .execute(&format!("DELETE FROM \"{table}\""), [])
                .map_err(map_sqlite_error)?;
        }
        Ok(removed)
    }

    /// The protected (platform-wrapped) MLS storage key, as the app stored it — one per database (the
    /// OpenMLS tables are not profile-scoped).
    pub fn get_mls_storage_key_protected(&self) -> OmniResult<Option<String>> {
        self.connection
            .query_row("SELECT protected_json FROM mls_storage_key WHERE id = 1", [], |row| row.get(0))
            .optional()
            .map_err(map_sqlite_error)
    }

    pub fn set_mls_storage_key_protected(&self, protected_json: &str) -> OmniResult<()> {
        self.connection
            .execute(
                "INSERT OR IGNORE INTO mls_storage_key (id, protected_json) VALUES (1, ?1)",
                params![protected_json],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }
}

fn map_group_record_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<GroupRecord> {
    Ok(GroupRecord {
        group_id: row.get(0)?,
        name: row.get(1)?,
        my_member_pub: row.get(2)?,
        my_status: row.get(3)?,
        dialog_key: row.get(4)?,
        created_at_unix_ms: row.get(5)?,
        updated_at_unix_ms: row.get(6)?,
    })
}

fn map_group_invite_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<GroupInviteRecord> {
    Ok(GroupInviteRecord {
        group_id: row.get(0)?,
        name: row.get(1)?,
        inviter_name: row.get(2)?,
        inviter_dialog_key: row.get(3)?,
        status: row.get(4)?,
        received_at_unix_ms: row.get(5)?,
    })
}

/// A group line as its author signed it — what goes into a newcomer's history packet
/// (docs/decisions/group-history-sync.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSignedLine {
    pub message_id: String,
    /// `text` or `attachment`.
    pub kind: String,
    pub sender_pub: String,
    pub sender_name: String,
    /// The body the author signed (for a file: the offer JSON).
    pub body: String,
    pub authored_at_unix_ms: i64,
    pub signature_b64: String,
}

impl Storage {
    /// Keep a signed line (ours or a member's). The same line twice is one.
    pub fn record_group_signed_line(&self, group_id: &str, line: &GroupSignedLine) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "INSERT OR IGNORE INTO group_signed_lines (
                    profile_id, group_id, message_id, kind, sender_pub, sender_name, body, authored_at_unix_ms, signature_b64
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    profile_id.0,
                    group_id,
                    line.message_id,
                    line.kind,
                    line.sender_pub,
                    line.sender_name,
                    line.body,
                    line.authored_at_unix_ms,
                    line.signature_b64
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// The latest `limit` signed lines of a group whose message is still in the chat, newest first.
    pub fn list_recent_group_signed_lines(&self, group_id: &str, limit: usize) -> OmniResult<Vec<GroupSignedLine>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT l.message_id, l.kind, l.sender_pub, l.sender_name, l.body, l.authored_at_unix_ms, l.signature_b64
                 FROM group_signed_lines l
                 WHERE l.profile_id = ?1 AND l.group_id = ?2
                   AND EXISTS (SELECT 1 FROM messages m WHERE m.id = l.message_id)
                 ORDER BY l.authored_at_unix_ms DESC, l.message_id DESC
                 LIMIT ?3",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0, group_id, limit as i64], |row| {
                Ok(GroupSignedLine {
                    message_id: row.get(0)?,
                    kind: row.get(1)?,
                    sender_pub: row.get(2)?,
                    sender_name: row.get(3)?,
                    body: row.get(4)?,
                    authored_at_unix_ms: row.get(5)?,
                    signature_b64: row.get(6)?,
                })
            })
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }
}

/// Our own Commits are kept this many epochs back: a branch that lost after that many epochs apart
/// re-issues them on the winning one (docs/decisions/group-history-sync.md, «Синхронизация эпох»).
const GROUP_OWN_CHANGES_KEPT_EPOCHS: i64 = 64;

/// A group that went on in two branches, as this device sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupBranchState {
    pub group_id: String,
    /// The earliest epoch a Commit of another branch was seen at, and whether ours won there.
    pub fork_epoch: u64,
    pub ours_wins: bool,
    pub first_seen_at_unix_ms: i64,
    /// Our branch lost: since when we ask to be put back, and when we last asked.
    pub rejoining_since_unix_ms: Option<i64>,
    pub requested_at_unix_ms: i64,
    /// The request we post (a KeyPackage as an MLS message) — the same one every time, so a copy of
    /// it still on its way once we are back matches the leaf we came back with.
    pub request_mls: Option<Vec<u8>>,
}

/// A member of another branch asking to be put back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupReaddRequest {
    pub group_id: String,
    pub member_pub: String,
    pub key_package: Vec<u8>,
    pub seen_at_unix_ms: i64,
}

impl Storage {
    /// Note a Commit of another branch at `epoch`; the earliest epoch seen is the one that counts.
    pub fn note_group_fork(&self, group_id: &str, epoch: u64, ours_wins: bool, now: i64) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "INSERT INTO group_branches (profile_id, group_id, fork_epoch, ours_wins, first_seen_at_unix_ms, requested_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0)
                 ON CONFLICT(profile_id, group_id) DO UPDATE SET
                    ours_wins = CASE WHEN excluded.fork_epoch < group_branches.fork_epoch THEN excluded.ours_wins ELSE group_branches.ours_wins END,
                    fork_epoch = MIN(group_branches.fork_epoch, excluded.fork_epoch)",
                params![profile_id.0, group_id, epoch as i64, ours_wins, now],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn list_group_branch_states(&self) -> OmniResult<Vec<GroupBranchState>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT group_id, fork_epoch, ours_wins, first_seen_at_unix_ms, rejoining_since_unix_ms, requested_at_unix_ms,
                        request_mls
                 FROM group_branches WHERE profile_id = ?1",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0], |row| {
                Ok(GroupBranchState {
                    group_id: row.get(0)?,
                    fork_epoch: row.get::<_, i64>(1)? as u64,
                    ours_wins: row.get(2)?,
                    first_seen_at_unix_ms: row.get(3)?,
                    rejoining_since_unix_ms: row.get(4)?,
                    requested_at_unix_ms: row.get(5)?,
                    request_mls: row.get(6)?,
                })
            })
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    pub fn get_group_branch_state(&self, group_id: &str) -> OmniResult<Option<GroupBranchState>> {
        Ok(self.list_group_branch_states()?.into_iter().find(|state| state.group_id == group_id))
    }

    pub fn mark_group_rejoining(&self, group_id: &str, since: i64) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "UPDATE group_branches SET rejoining_since_unix_ms = COALESCE(rejoining_since_unix_ms, ?3)
                 WHERE profile_id = ?1 AND group_id = ?2",
                params![profile_id.0, group_id, since],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn mark_group_rejoin_requested(&self, group_id: &str, now: i64, request_mls: &[u8]) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "UPDATE group_branches SET requested_at_unix_ms = ?3, request_mls = ?4 WHERE profile_id = ?1 AND group_id = ?2",
                params![profile_id.0, group_id, now, request_mls],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn clear_group_branch_state(&self, group_id: &str) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "DELETE FROM group_branches WHERE profile_id = ?1 AND group_id = ?2",
                params![profile_id.0, group_id],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// Keep a member's request to be put back (the latest KeyPackage replaces an older one).
    pub fn record_group_readd_request(&self, request: &GroupReaddRequest) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "INSERT INTO group_readd_requests (profile_id, group_id, member_pub, key_package, seen_at_unix_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(profile_id, group_id, member_pub) DO UPDATE SET
                    key_package = excluded.key_package",
                params![profile_id.0, request.group_id, request.member_pub, request.key_package, request.seen_at_unix_ms],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn list_group_readd_requests(&self) -> OmniResult<Vec<GroupReaddRequest>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT group_id, member_pub, key_package, seen_at_unix_ms FROM group_readd_requests WHERE profile_id = ?1",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0], |row| {
                Ok(GroupReaddRequest {
                    group_id: row.get(0)?,
                    member_pub: row.get(1)?,
                    key_package: row.get(2)?,
                    seen_at_unix_ms: row.get(3)?,
                })
            })
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    /// Forget every request to put someone back in this group (we just came in ourselves: what was
    /// asked before is from before; a member still out asks again).
    pub fn drop_group_readd_requests(&self, group_id: &str) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "DELETE FROM group_readd_requests WHERE profile_id = ?1 AND group_id = ?2",
                params![profile_id.0, group_id],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn drop_group_readd_request(&self, group_id: &str, member_pub: &str) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "DELETE FROM group_readd_requests WHERE profile_id = ?1 AND group_id = ?2 AND member_pub = ?3",
                params![profile_id.0, group_id, member_pub],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// Our own Commits after `epoch` (oldest first), taken out: to be made again on the winning branch.
    pub fn take_group_own_changes_after(&self, group_id: &str, epoch: u64) -> OmniResult<Vec<(u64, String)>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT epoch_after, change_json FROM group_own_changes
                 WHERE profile_id = ?1 AND group_id = ?2 AND epoch_after > ?3 ORDER BY epoch_after",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0, group_id, epoch as i64], |row| {
                Ok((row.get::<_, i64>(0)? as u64, row.get::<_, String>(1)?))
            })
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "DELETE FROM group_own_changes WHERE profile_id = ?1 AND group_id = ?2 AND epoch_after > ?3",
                params![profile_id.0, group_id, epoch as i64],
            )
            .map_err(map_sqlite_error)?;
        Ok(rows)
    }
}

/// A Welcome for a member we put back, held until our Commit is sure to stand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupPendingWelcome {
    pub group_id: String,
    pub member_pub: String,
    /// The epoch our Commit was made for, and its rank: if another branch's Commit wins that epoch,
    /// this Welcome leads nowhere and is dropped.
    pub epoch_before: u64,
    pub commit_rank: Vec<u8>,
    pub welcome: Vec<u8>,
    pub created_at_unix_ms: i64,
}

impl Storage {
    pub fn hold_group_welcome(&self, pending: &GroupPendingWelcome) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "INSERT OR REPLACE INTO group_pending_welcomes (
                    profile_id, group_id, member_pub, epoch_before, commit_rank, welcome, created_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    profile_id.0,
                    pending.group_id,
                    pending.member_pub,
                    pending.epoch_before as i64,
                    pending.commit_rank,
                    pending.welcome,
                    pending.created_at_unix_ms
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn list_group_pending_welcomes(&self) -> OmniResult<Vec<GroupPendingWelcome>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT group_id, member_pub, epoch_before, commit_rank, welcome, created_at_unix_ms
                 FROM group_pending_welcomes WHERE profile_id = ?1",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0], |row| {
                Ok(GroupPendingWelcome {
                    group_id: row.get(0)?,
                    member_pub: row.get(1)?,
                    epoch_before: row.get::<_, i64>(2)? as u64,
                    commit_rank: row.get(3)?,
                    welcome: row.get(4)?,
                    created_at_unix_ms: row.get(5)?,
                })
            })
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite_error)
    }

    pub fn drop_group_pending_welcome(&self, group_id: &str, member_pub: &str) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.connection
            .execute(
                "DELETE FROM group_pending_welcomes WHERE profile_id = ?1 AND group_id = ?2 AND member_pub = ?3",
                params![profile_id.0, group_id, member_pub],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }
}
