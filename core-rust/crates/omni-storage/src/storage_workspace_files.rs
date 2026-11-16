// Latest authenticated file-workspace events, plus strictly local paths and download requests.
// Authorization and event keys are supplied by omni-app after checking the MLS sender.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceFileEventRecord {
    pub sender_account: String,
    pub sender_member: String,
    pub event_json: String,
    pub at_unix_ms: i64,
}

/// How many events of one kind (locks, revisions, lines of projects, …) one member has in a group's
/// workspace; past it the oldest go.
pub const WORKSPACE_FILE_EVENTS_PER_SENDER_KIND_MAX: i64 = 1024;

/// How many of one reviewer's reviews of a project are kept: those that did not accept, and those
/// that did.
pub const WORKSPACE_REVIEWS_OPEN_MAX: i64 = 256;
pub const WORKSPACE_REVIEWS_ACCEPTED_MAX: i64 = 4096;

impl Storage {
    pub fn put_workspace_file_event(
        &self,
        group: &str,
        key: &str,
        event: &WorkspaceFileEventRecord,
    ) -> OmniResult<bool> {
        let profile = self.require_active_profile_id()?;
        let count = self.connection.execute(
            "INSERT INTO workspace_file_events (profile_id,group_id,event_key,sender_account,sender_member,event_json,at_unix_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(profile_id,group_id,event_key) DO UPDATE SET
               sender_account=excluded.sender_account,sender_member=excluded.sender_member,
               event_json=excluded.event_json,at_unix_ms=excluded.at_unix_ms
             WHERE (excluded.at_unix_ms,excluded.event_json,excluded.sender_member) >
                   (workspace_file_events.at_unix_ms,workspace_file_events.event_json,workspace_file_events.sender_member)",
            params![profile.0,group,key,event.sender_account,event.sender_member,event.event_json,event.at_unix_ms],
        ).map_err(map_sqlite_error)?;
        if let Some(repo) = key
            .strip_prefix("review:")
            .or_else(|| key.strip_prefix("pending_review:"))
            .and_then(|tail| tail.split(':').next())
        {
            let prefix = format!("{}:{repo}:", key.split(':').next().unwrap_or("review"));
            // «Accepted» and the rest are bounded apart: under one bound, a reviewer's «needs
            // changes» on other versions pushed out its own «accepted» — which it cannot take back
            // by saying so (blind audit 2026-11-11, pass 14).
            for (accepted, kept) in [(0_i64, WORKSPACE_REVIEWS_OPEN_MAX), (1, WORKSPACE_REVIEWS_ACCEPTED_MAX)] {
                self.connection.execute(
                    "DELETE FROM workspace_file_events WHERE profile_id=?1 AND group_id=?2 AND sender_account=?3
                       AND substr(event_key,1,?4)=?5
                       AND COALESCE(json_extract(event_json,'$.accepted'),json_extract(event_json,'$.deferred_review.accepted'),0)=?6
                       AND event_key NOT IN
                         (SELECT event_key FROM workspace_file_events WHERE profile_id=?1 AND group_id=?2 AND sender_account=?3
                            AND substr(event_key,1,?4)=?5
                            AND COALESCE(json_extract(event_json,'$.accepted'),json_extract(event_json,'$.deferred_review.accepted'),0)=?6
                            ORDER BY at_unix_ms DESC,event_key DESC LIMIT ?7)",
                    params![profile.0,group,event.sender_account,prefix.len() as i64,prefix,accepted,kept],
                ).map_err(map_sqlite_error)?;
            }
        } else if count > 0 {
            // Every other kind of event has a key its sender chooses a part of (a project, a
            // version, a file): one member's events of one kind are kept up to a bound, the newest
            // (blind audit 2026-11-11, pass 13 — revisions, «навсегда», locks, lines of projects
            // grew without one).
            let kind = format!("{}:", key.split(':').next().unwrap_or_default());
            self.connection.execute(
                "DELETE FROM workspace_file_events WHERE profile_id=?1 AND group_id=?2 AND sender_account=?3
                   AND substr(event_key,1,?4)=?5 AND event_key NOT IN
                     (SELECT event_key FROM workspace_file_events WHERE profile_id=?1 AND group_id=?2 AND sender_account=?3
                        AND substr(event_key,1,?4)=?5 ORDER BY at_unix_ms DESC,event_key DESC LIMIT ?6)",
                params![profile.0,group,event.sender_account,kind.len() as i64,kind,WORKSPACE_FILE_EVENTS_PER_SENDER_KIND_MAX],
            ).map_err(map_sqlite_error)?;
        }
        Ok(count > 0)
    }

    pub fn list_workspace_file_events(
        &self,
        group: &str,
    ) -> OmniResult<Vec<WorkspaceFileEventRecord>> {
        let profile = self.require_active_profile_id()?;
        let mut stmt = self.connection.prepare(
            "SELECT sender_account,sender_member,event_json,at_unix_ms FROM workspace_file_events
             WHERE profile_id=?1 AND group_id=?2 ORDER BY event_key"
        ).map_err(map_sqlite_error)?;
        let rows = stmt
            .query_map(params![profile.0, group], |row| {
                Ok(WorkspaceFileEventRecord {
                    sender_account: row.get(0)?,
                    sender_member: row.get(1)?,
                    event_json: row.get(2)?,
                    at_unix_ms: row.get(3)?,
                })
            })
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)
    }

    pub fn set_workspace_file_local(
        &self,
        group: &str,
        repo: &str,
        setting: &str,
        value: &str,
    ) -> OmniResult<()> {
        let profile = self.require_active_profile_id()?;
        self.connection.execute(
            "INSERT INTO workspace_file_local (profile_id,group_id,repo,setting,value) VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT(profile_id,group_id,repo,setting) DO UPDATE SET value=excluded.value",
            params![profile.0,group,repo,setting,value],
        ).map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn get_workspace_file_local(
        &self,
        group: &str,
        repo: &str,
        setting: &str,
    ) -> OmniResult<Option<String>> {
        let profile = self.require_active_profile_id()?;
        self.connection.query_row(
            "SELECT value FROM workspace_file_local WHERE profile_id=?1 AND group_id=?2 AND repo=?3 AND setting=?4",
            params![profile.0,group,repo,setting],|row| row.get(0),
        ).optional().map_err(map_sqlite_error)
    }
}
