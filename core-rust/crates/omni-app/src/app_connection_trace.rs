// Tier-2 honest connection trace: a per-contact ring of the connection-attempt STEPS the
// orchestration layer actually performs (LAN dial → rendezvous → linked), with outcomes +
// timestamps, so the UI can show HOW a connection is happening and WHERE it stalled — not just a
// snapshot verdict. Recorded by the autoconnect worker / rendezvous sweep (the real attempt points),
// read by `mesh_connection_state` into the DTO. `include!`d into the crate module, so it shares scope
// (calls `current_unix_ms()` directly). Keyed by (db_path, account_id) and cleared in
// `stop_runtime_context` — see CONTRIBUTING.md §Static registries. No wire/storage change.
//
// NOTE: this file is `include!`d into the crate root, which already imports some names (e.g.
// `Mutex`). To avoid duplicate-import errors we use fully-qualified `std::` paths and add no
// top-level `use`. This is a live read-model (in-memory, bounded), NOT history.

/// Keep the last N steps per contact — enough to show the journey (LAN → rendezvous → linked)
/// without unbounded growth as the worker sweeps every few seconds.
const MAX_CONNECTION_STEPS: usize = 12;

/// Which rung of the connectivity ladder a step belongs to. Mirrors the dial ladder the
/// orchestration layer drives; the shell maps each to a human line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStage {
    /// LAN multicast announce + direct dial on the local network.
    Lan,
    /// Serverless rendezvous: publish a blinded beacon + dial the peer's reflexive endpoint.
    Rendezvous,
    /// A verified mesh link is up (terminal success).
    Linked,
}

/// Outcome of a step at the moment it was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStepStatus {
    /// Attempt in flight (e.g. dialing / publishing a beacon).
    Started,
    /// Succeeded (e.g. link established).
    Ok,
    /// Failed for the recorded `detail` reason.
    Failed,
    /// Deliberately not attempted (e.g. direct-only privacy, no shared secret yet).
    Skipped,
}

/// One step in a contact's connection journey.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConnectionStep {
    pub stage: ConnectionStage,
    pub status: ConnectionStepStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub at_ms: i64,
}

/// db_path → account_id → recent steps (bounded ring).
type ConnectionTraceMap =
    std::collections::HashMap<String, std::collections::HashMap<String, std::collections::VecDeque<ConnectionStep>>>;

static CONNECTION_TRACES: std::sync::OnceLock<std::sync::Mutex<ConnectionTraceMap>> =
    std::sync::OnceLock::new();

fn connection_traces() -> &'static std::sync::Mutex<ConnectionTraceMap> {
    CONNECTION_TRACES.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Record (or refresh) one connection step for a contact. Consecutive identical steps
/// (same stage+status+detail) only refresh the timestamp instead of growing the ring — the worker
/// re-attempts every few seconds, so "publishing rendezvous beacon…" should read as one live step
/// whose "… Nс назад" updates, not a flood of duplicates.
fn record_connection_step(
    db_path: &str,
    account_id: &str,
    stage: ConnectionStage,
    status: ConnectionStepStatus,
    detail: Option<String>,
) {
    let at_ms = current_unix_ms();
    let Ok(mut map) = connection_traces().lock() else {
        return;
    };
    let ring = map
        .entry(db_path.to_string())
        .or_default()
        .entry(account_id.to_string())
        .or_default();
    if let Some(last) = ring.back_mut() {
        if last.stage == stage && last.status == status && last.detail == detail {
            last.at_ms = at_ms;
            return;
        }
    }
    ring.push_back(ConnectionStep { stage, status, detail, at_ms });
    while ring.len() > MAX_CONNECTION_STEPS {
        ring.pop_front();
    }
}

/// The recent connection steps for a contact (oldest → newest), for the connection-state DTO.
fn connection_trace_for(db_path: &str, account_id: &str) -> Vec<ConnectionStep> {
    connection_traces()
        .lock()
        .ok()
        .and_then(|map| {
            map.get(db_path)
                .and_then(|per_db| per_db.get(account_id))
                .map(|ring| ring.iter().cloned().collect())
        })
        .unwrap_or_default()
}

/// Drop a profile's traces (called from `stop_runtime_context`).
fn clear_connection_traces(db_path: &str) {
    if let Ok(mut map) = connection_traces().lock() {
        map.remove(db_path);
    }
}

#[cfg(test)]
mod connection_trace_tests {
    use super::*;

    #[test]
    fn records_and_reads_steps_in_order() {
        let db = "trace-test-db-order";
        clear_connection_traces(db);
        record_connection_step(db, "acct-x", ConnectionStage::Lan, ConnectionStepStatus::Started, None);
        record_connection_step(
            db,
            "acct-x",
            ConnectionStage::Rendezvous,
            ConnectionStepStatus::Started,
            Some("маячок".to_string()),
        );
        record_connection_step(db, "acct-x", ConnectionStage::Linked, ConnectionStepStatus::Ok, None);
        let steps = connection_trace_for(db, "acct-x");
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].stage, ConnectionStage::Lan);
        assert_eq!(steps[2].stage, ConnectionStage::Linked);
        assert_eq!(steps[2].status, ConnectionStepStatus::Ok);
        clear_connection_traces(db);
    }

    #[test]
    fn identical_consecutive_steps_collapse_and_refresh() {
        let db = "trace-test-db-dedup";
        clear_connection_traces(db);
        record_connection_step(db, "a", ConnectionStage::Rendezvous, ConnectionStepStatus::Started, None);
        record_connection_step(db, "a", ConnectionStage::Rendezvous, ConnectionStepStatus::Started, None);
        record_connection_step(db, "a", ConnectionStage::Rendezvous, ConnectionStepStatus::Started, None);
        assert_eq!(connection_trace_for(db, "a").len(), 1, "duplicates collapse to one live step");
        clear_connection_traces(db);
    }

    #[test]
    fn ring_is_bounded() {
        let db = "trace-test-db-bound";
        clear_connection_traces(db);
        for i in 0..(MAX_CONNECTION_STEPS + 6) {
            let stage = if i % 2 == 0 { ConnectionStage::Lan } else { ConnectionStage::Rendezvous };
            record_connection_step(db, "a", stage, ConnectionStepStatus::Started, Some(i.to_string()));
        }
        assert_eq!(connection_trace_for(db, "a").len(), MAX_CONNECTION_STEPS);
        clear_connection_traces(db);
    }

    #[test]
    fn clear_removes_profile_traces() {
        let db = "trace-test-db-clear";
        record_connection_step(db, "a", ConnectionStage::Lan, ConnectionStepStatus::Started, None);
        clear_connection_traces(db);
        assert!(connection_trace_for(db, "a").is_empty());
    }
}
