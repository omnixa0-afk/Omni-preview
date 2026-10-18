#[derive(Debug, Clone, Default)]
struct RuntimeChatSessionEntry {
    is_open_lease: bool,
    auto_reconnect_paused: bool,
    // When the open lease last (re)armed: opened, resumed, or re-confirmed by a live connection.
    // Messages sent before it wait for a fresh keepalive (`..._waits_for_keepalive`).
    lease_triggered_at_unix_ms: Option<i64>,
    last_interaction_at_unix_ms: Option<i64>,
    last_interaction_kind: Option<omni_types::ChatSessionInteractionKind>,
    request_signal_observed: bool,
    request_signal_sent: bool,
    approval_signal_pending: bool,
    last_confirmed_keepalive_at_unix_ms: Option<i64>,
    peer_endpoint: Option<String>,
    local_endpoint: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct RuntimeChatSessionRegistry {
    entries: std::collections::HashMap<String, RuntimeChatSessionEntry>,
    paused_activation_allowed_until_unix_ms: Option<i64>,
}

#[derive(Debug, Clone)]
struct OpenChatLeaseSnapshot {
    dialog_key: String,
    is_open_lease: bool,
    auto_reconnect_paused: bool,
}

static RUNTIME_CHAT_SESSIONS:
    OnceLock<Mutex<std::collections::HashMap<String, RuntimeChatSessionRegistry>>> = OnceLock::new();

const CHAT_SESSION_KEEPALIVE_STALE_AFTER_MS: i64 = 40_000;
// Upper bound on how long the open-lease send gate will keep deferring a queued message while it
// waits for a keepalive round-trip to re-confirm liveness. Past this ceiling the keepalive is never
// going to confirm — the classic case is an asymmetric link where our outbound leg is dead (mobile
// carrier drops the UDP flow one-way) so our ping never reaches the peer and no pong ever returns.
// Deferring forever black-holes the message; instead we stop deferring and let the send path run
// (its durable store-and-forward backstop + phantom-link drop then take over). Two full stale
// windows give a genuinely-slow-but-alive link a fair chance before we give up. See
// docs/decisions/delivery-reliability-model.md and docs/decisions/open-lease-keepalive-wait.md.
const CHAT_SESSION_KEEPALIVE_WAIT_CEILING_MS: i64 = 2 * CHAT_SESSION_KEEPALIVE_STALE_AFTER_MS;

fn runtime_chat_session_registry(
) -> &'static Mutex<std::collections::HashMap<String, RuntimeChatSessionRegistry>> {
    RUNTIME_CHAT_SESSIONS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn lock_runtime_chat_session_registry() -> OmniResult<
    std::sync::MutexGuard<'static, std::collections::HashMap<String, RuntimeChatSessionRegistry>>,
> {
    runtime_chat_session_registry()
        .lock()
        .map_err(|_| OmniError::Internal("runtime chat session registry lock is poisoned".to_string()))
}

fn active_profile_runtime_chat_session_key(storage: &Storage) -> String {
    storage.db_path().to_string_lossy().to_string()
}

fn session_capable_dialog(storage: &Storage, dialog_key: &str) -> OmniResult<bool> {
    if instance_id_from_dialog_key(dialog_key).is_some() {
        return Ok(true);
    }

    Ok(storage
        .resolve_contact_for_dialog(dialog_key)?
        .is_some_and(|contact| {
            contact.relationship_state == ContactRelationshipState::Approved
                && contact
                    .remote_instance_id
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty())
        }))
}

fn touch_runtime_chat_session_interaction(
    db_path: &str,
    dialog_key: &str,
    kind: omni_types::ChatSessionInteractionKind,
    observed_at_unix_ms: i64,
) -> OmniResult<()> {
    let dialog_key = normalize_dialog_key(dialog_key);
    if dialog_key.trim().is_empty() {
        return Ok(());
    }

    let mut registry = lock_runtime_chat_session_registry()?;
    let entry = registry
        .entry(db_path.to_string())
        .or_default()
        .entries
        .entry(dialog_key)
        .or_default();
    entry.last_interaction_at_unix_ms = Some(observed_at_unix_ms);
    entry.last_interaction_kind = Some(kind);
    Ok(())
}

fn set_runtime_chat_session_confirmed_keepalive_at(
    db_path: &str,
    dialog_key: &str,
    observed_at_unix_ms: i64,
) -> OmniResult<()> {
    let dialog_key = normalize_dialog_key(dialog_key);
    if dialog_key.trim().is_empty() {
        return Ok(());
    }
    let mut registry = lock_runtime_chat_session_registry()?;
    let entry = registry
        .entry(db_path.to_string())
        .or_default()
        .entries
        .entry(dialog_key)
        .or_default();
    entry.last_confirmed_keepalive_at_unix_ms = Some(observed_at_unix_ms.max(0));
    Ok(())
}

fn mark_runtime_chat_session_request_signal_observed(
    db_path: &str,
    dialog_key: &str,
) -> OmniResult<()> {
    let dialog_key = normalize_dialog_key(dialog_key);
    if dialog_key.trim().is_empty() {
        return Ok(());
    }

    let mut registry = lock_runtime_chat_session_registry()?;
    let entry = registry
        .entry(db_path.to_string())
        .or_default()
        .entries
        .entry(dialog_key)
        .or_default();
    entry.request_signal_observed = true;
    Ok(())
}

fn mark_runtime_chat_session_request_signal_sent(
    db_path: &str,
    dialog_key: &str,
) -> OmniResult<()> {
    let dialog_key = normalize_dialog_key(dialog_key);
    if dialog_key.trim().is_empty() {
        return Ok(());
    }

    let mut registry = lock_runtime_chat_session_registry()?;
    let entry = registry
        .entry(db_path.to_string())
        .or_default()
        .entries
        .entry(dialog_key)
        .or_default();
    entry.request_signal_sent = true;
    Ok(())
}

fn mark_runtime_chat_session_approval_signal_pending(
    db_path: &str,
    dialog_key: &str,
) -> OmniResult<()> {
    let dialog_key = normalize_dialog_key(dialog_key);
    if dialog_key.trim().is_empty() {
        return Ok(());
    }

    let mut registry = lock_runtime_chat_session_registry()?;
    let entry = registry
        .entry(db_path.to_string())
        .or_default()
        .entries
        .entry(dialog_key)
        .or_default();
    entry.approval_signal_pending = true;
    Ok(())
}

fn clear_runtime_chat_session_approval_signal_pending(
    db_path: &str,
    dialog_key: &str,
) -> OmniResult<()> {
    let dialog_key = normalize_dialog_key(dialog_key);
    if dialog_key.trim().is_empty() {
        return Ok(());
    }

    let mut registry = lock_runtime_chat_session_registry()?;
    if let Some(entry) = registry
        .get_mut(db_path)
        .and_then(|profile_registry| profile_registry.entries.get_mut(&dialog_key))
    {
        entry.approval_signal_pending = false;
    }
    Ok(())
}

fn runtime_chat_session_approval_signal_pending(
    db_path: &str,
    dialog_key: &str,
) -> OmniResult<bool> {
    let dialog_key = normalize_dialog_key(dialog_key);
    if dialog_key.trim().is_empty() {
        return Ok(false);
    }
    let registry = lock_runtime_chat_session_registry()?;
    Ok(registry
        .get(db_path)
        .and_then(|profile_registry| profile_registry.entries.get(&dialog_key))
        .is_some_and(|entry| entry.approval_signal_pending))
}

pub(crate) fn runtime_chat_session_request_signal_observed(
    db_path: &str,
    dialog_key: &str,
) -> OmniResult<bool> {
    let dialog_key = normalize_dialog_key(dialog_key);
    if dialog_key.trim().is_empty() {
        return Ok(false);
    }
    let registry = lock_runtime_chat_session_registry()?;
    Ok(registry
        .get(db_path)
        .and_then(|profile_registry| profile_registry.entries.get(&dialog_key))
        .is_some_and(|entry| entry.request_signal_observed))
}

pub(crate) fn runtime_chat_session_request_signal_sent(
    db_path: &str,
    dialog_key: &str,
) -> OmniResult<bool> {
    let dialog_key = normalize_dialog_key(dialog_key);
    if dialog_key.trim().is_empty() {
        return Ok(false);
    }
    let registry = lock_runtime_chat_session_registry()?;
    Ok(registry
        .get(db_path)
        .and_then(|profile_registry| profile_registry.entries.get(&dialog_key))
        .is_some_and(|entry| entry.request_signal_sent))
}

pub(crate) fn runtime_chat_session_last_interaction_kind(
    db_path: &str,
    dialog_key: &str,
) -> OmniResult<Option<omni_types::ChatSessionInteractionKind>> {
    let dialog_key = normalize_dialog_key(dialog_key);
    if dialog_key.trim().is_empty() {
        return Ok(None);
    }
    let registry = lock_runtime_chat_session_registry()?;
    Ok(registry
        .get(db_path)
        .and_then(|profile_registry| profile_registry.entries.get(&dialog_key))
        .and_then(|entry| entry.last_interaction_kind))
}

fn runtime_chat_session_open_lease_waits_for_keepalive(
    db_path: &str,
    dialog_key: &str,
    now_unix_ms: i64,
) -> OmniResult<bool> {
    let dialog_key = normalize_dialog_key(dialog_key);
    if dialog_key.trim().is_empty() {
        return Ok(false);
    }
    let registry = lock_runtime_chat_session_registry()?;
    let Some(entry) = registry
        .get(db_path)
        .and_then(|profile_registry| profile_registry.entries.get(&dialog_key))
    else {
        return Ok(false);
    };
    if !entry.is_open_lease || entry.auto_reconnect_paused {
        return Ok(false);
    }

    // Confirmed peer keepalive is the strongest evidence of liveness. When it
    // is absent (brand-new lease that never completed a keepalive round-trip),
    // fall back to the last local interaction timestamp so the very first send
    // on a freshly opened lease is not artificially deferred. The QUIC-level
    // `connection_snapshot_for_dialog` check at the caller already confirms
    // there is a live carrier; this app-layer wait exists to catch sessions
    // that have been silent for a long time, not freshly opened ones.
    // See docs/decisions/open-lease-keepalive-wait.md.
    let reference_at_unix_ms = entry
        .last_confirmed_keepalive_at_unix_ms
        .or(entry.last_interaction_at_unix_ms);

    Ok(reference_at_unix_ms
        .map(|observed_at_unix_ms| {
            // Defer only inside the [stale, ceiling) window. Below `stale` the session is fresh
            // enough to send optimistically; at/above `ceiling` the keepalive is never going to
            // confirm (one-way-dead link), so we STOP deferring and fall through to the real send,
            // whose durable backstop + phantom-link drop rescue the message. Deferring past the
            // ceiling would black-hole it forever.
            let age = now_unix_ms.saturating_sub(observed_at_unix_ms);
            (CHAT_SESSION_KEEPALIVE_STALE_AFTER_MS..CHAT_SESSION_KEEPALIVE_WAIT_CEILING_MS)
                .contains(&age)
        })
        .unwrap_or(true))
}

fn runtime_chat_session_should_bypass_keepalive_throttle(
    db_path: &str,
    dialog_key: &str,
    now_unix_ms: i64,
    last_sent_at_unix_ms: i64,
) -> OmniResult<bool> {
    let dialog_key = normalize_dialog_key(dialog_key);
    if dialog_key.trim().is_empty() {
        return Ok(false);
    }
    let registry = lock_runtime_chat_session_registry()?;
    let Some(entry) = registry
        .get(db_path)
        .and_then(|profile_registry| profile_registry.entries.get(&dialog_key))
    else {
        return Ok(false);
    };
    if !entry.is_open_lease || entry.auto_reconnect_paused {
        return Ok(false);
    }
    let waits_for_keepalive = entry
        .last_confirmed_keepalive_at_unix_ms
        .map(|observed_at_unix_ms| {
            now_unix_ms.saturating_sub(observed_at_unix_ms)
                >= CHAT_SESSION_KEEPALIVE_STALE_AFTER_MS
        })
        .unwrap_or(true);
    if !waits_for_keepalive {
        return Ok(false);
    }

    let lease_triggered_at_unix_ms = entry
        .last_interaction_at_unix_ms
        .or(entry.lease_triggered_at_unix_ms)
        .unwrap_or_default();
    Ok(last_sent_at_unix_ms <= 0 || last_sent_at_unix_ms < lease_triggered_at_unix_ms)
}

fn set_runtime_chat_session_endpoints(
    db_path: &str,
    dialog_key: &str,
    local_endpoint: Option<&str>,
    peer_endpoint: Option<&str>,
) -> OmniResult<()> {
    let dialog_key = normalize_dialog_key(dialog_key);
    if dialog_key.trim().is_empty() {
        return Ok(());
    }
    let mut registry = lock_runtime_chat_session_registry()?;
    let entry = registry
        .entry(db_path.to_string())
        .or_default()
        .entries
        .entry(dialog_key)
        .or_default();
    entry.local_endpoint = local_endpoint
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    entry.peer_endpoint = peer_endpoint
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    Ok(())
}

fn mark_runtime_chat_session_lease_triggered(
    db_path: &str,
    dialog_key: &str,
) -> OmniResult<()> {
    let dialog_key = normalize_dialog_key(dialog_key);
    if dialog_key.trim().is_empty() {
        return Ok(());
    }
    let mut registry = lock_runtime_chat_session_registry()?;
    if let Some(entry) = registry
        .entry(db_path.to_string())
        .or_default()
        .entries
        .get_mut(&dialog_key)
    {
        entry.lease_triggered_at_unix_ms = Some(current_unix_ms());
    }
    Ok(())
}

fn open_chat_session_lease_for_profile(
    storage: &Storage,
    dialog_key_or_selector: &str,
) -> OmniResult<String> {
    let canonical_dialog_key =
        resolve_canonical_dialog_key_for_selector(storage, dialog_key_or_selector.trim())?;
    let db_path = active_profile_runtime_chat_session_key(storage);
    let mut registry = lock_runtime_chat_session_registry()?;
    let entry = registry
        .entry(db_path)
        .or_default()
        .entries
        .entry(canonical_dialog_key.clone())
        .or_default();
    entry.is_open_lease = true;
    if !entry.auto_reconnect_paused {
        entry.lease_triggered_at_unix_ms = Some(current_unix_ms());
    }
    Ok(canonical_dialog_key)
}

fn clear_chat_session_pause_for_profile(
    storage: &Storage,
    dialog_key_or_selector: &str,
) -> OmniResult<String> {
    let canonical_dialog_key =
        resolve_canonical_dialog_key_for_selector(storage, dialog_key_or_selector.trim())?;
    let db_path = active_profile_runtime_chat_session_key(storage);
    let mut registry = lock_runtime_chat_session_registry()?;
    let entry = registry
        .entry(db_path)
        .or_default()
        .entries
        .entry(canonical_dialog_key.clone())
        .or_default();
    entry.auto_reconnect_paused = false;
    if entry.is_open_lease {
        entry.lease_triggered_at_unix_ms = Some(current_unix_ms());
    }
    Ok(canonical_dialog_key)
}

fn resume_chat_session_lease_for_profile(
    storage: &Storage,
    dialog_key_or_selector: &str,
) -> OmniResult<String> {
    let canonical_dialog_key =
        resolve_canonical_dialog_key_for_selector(storage, dialog_key_or_selector.trim())?;
    let db_path = active_profile_runtime_chat_session_key(storage);
    let mut registry = lock_runtime_chat_session_registry()?;
    let entry = registry
        .entry(db_path)
        .or_default()
        .entries
        .entry(canonical_dialog_key.clone())
        .or_default();
    entry.is_open_lease = true;
    entry.auto_reconnect_paused = false;
    entry.lease_triggered_at_unix_ms = Some(current_unix_ms());
    Ok(canonical_dialog_key)
}

fn pause_chat_session_for_profile(
    storage: &Storage,
    dialog_key_or_selector: &str,
) -> OmniResult<String> {
    let canonical_dialog_key =
        resolve_canonical_dialog_key_for_selector(storage, dialog_key_or_selector.trim())?;
    let db_path = active_profile_runtime_chat_session_key(storage);
    let mut registry = lock_runtime_chat_session_registry()?;
    let profile_registry = registry.entry(db_path).or_default();
    let entry = profile_registry
        .entries
        .entry(canonical_dialog_key.clone())
        .or_default();
    entry.is_open_lease = false;
    entry.auto_reconnect_paused = true;
    entry.lease_triggered_at_unix_ms = None;
    entry.peer_endpoint = None;
    entry.local_endpoint = None;
    profile_registry.paused_activation_allowed_until_unix_ms = None;
    Ok(canonical_dialog_key)
}

fn runtime_chat_session_auto_reconnect_paused(
    db_path: &str,
    dialog_key: &str,
) -> OmniResult<bool> {
    let dialog_key = normalize_dialog_key(dialog_key);
    if dialog_key.trim().is_empty() {
        return Ok(false);
    }

    let registry = lock_runtime_chat_session_registry()?;
    Ok(registry
        .get(db_path)
        .and_then(|profile_registry| profile_registry.entries.get(&dialog_key))
        .is_some_and(|entry| entry.auto_reconnect_paused))
}

// Orphaned 2026-06-01 by the legacy-signaling removal (was its only trigger).
// Kept (dead) pending a decision on whether mesh chat needs a paused-activation
// allowance window; the reader `runtime_chat_session_paused_activation_allowed`
// remains, so removing this would silently neuter that path.
#[allow(dead_code)]
fn allow_paused_chat_session_activation_for_profile(
    storage: &Storage,
    allowed_until_unix_ms: i64,
) -> OmniResult<()> {
    let db_path = active_profile_runtime_chat_session_key(storage);
    let mut registry = lock_runtime_chat_session_registry()?;
    let profile_registry = registry.entry(db_path).or_default();
    profile_registry.paused_activation_allowed_until_unix_ms = Some(
        profile_registry
            .paused_activation_allowed_until_unix_ms
            .unwrap_or_default()
            .max(allowed_until_unix_ms),
    );
    Ok(())
}

fn runtime_chat_session_paused_activation_allowed(
    db_path: &str,
    now_unix_ms: i64,
) -> OmniResult<bool> {
    let registry = lock_runtime_chat_session_registry()?;
    Ok(registry
        .get(db_path)
        .and_then(|profile_registry| profile_registry.paused_activation_allowed_until_unix_ms)
        .is_some_and(|allowed_until_unix_ms| allowed_until_unix_ms >= now_unix_ms))
}

fn clear_runtime_chat_session_for_dialog_key(
    storage: &Storage,
    dialog_key: &str,
) -> OmniResult<()> {
    let dialog_key = normalize_dialog_key(dialog_key);
    if dialog_key.trim().is_empty() {
        return Ok(());
    }
    let db_path = active_profile_runtime_chat_session_key(storage);
    let mut registry = lock_runtime_chat_session_registry()?;
    if let Some(profile_registry) = registry.get_mut(&db_path) {
        profile_registry.entries.remove(&dialog_key);
        if profile_registry.entries.is_empty() {
            registry.remove(&db_path);
        }
    }
    Ok(())
}

fn clear_runtime_chat_session_for_contact(
    storage: &Storage,
    contact: &ContactSnapshot,
) -> OmniResult<()> {
    let mut dialog_keys = std::collections::HashSet::new();
    if let Some(primary_dialog_key) = contact.primary_dialog_key.as_deref() {
        dialog_keys.insert(normalize_dialog_key(primary_dialog_key));
    }
    if let Some(remote_instance_id) = contact.remote_instance_id.as_deref() {
        dialog_keys.insert(dialog_key_from_instance_id(remote_instance_id));
    }
    if let Some(remote_account_id) = contact.remote_account_id.as_ref() {
        dialog_keys.insert(omni_types::dialog_key_from_account_id(&remote_account_id.0));
    }

    for dialog_key in dialog_keys {
        clear_runtime_chat_session_for_dialog_key(storage, &dialog_key)?;
    }
    if let Some(remote_account_id) = contact.remote_account_id.as_ref() {
        forget_session_reachability_for_remote_account(
            &storage.db_path().to_string_lossy(),
            &remote_account_id.0,
        );
    }
    Ok(())
}

fn clear_runtime_chat_sessions_for_active_profile(storage: &Storage) -> OmniResult<()> {
    let db_path = active_profile_runtime_chat_session_key(storage);
    let mut registry = lock_runtime_chat_session_registry()?;
    registry.remove(&db_path);
    Ok(())
}

fn migrate_runtime_chat_session_dialog_key(
    db_path: &str,
    source_dialog_key: &str,
    target_dialog_key: &str,
) -> OmniResult<()> {
    let source_dialog_key = normalize_dialog_key(source_dialog_key);
    let target_dialog_key = normalize_dialog_key(target_dialog_key);
    if source_dialog_key.trim().is_empty()
        || target_dialog_key.trim().is_empty()
        || source_dialog_key == target_dialog_key
    {
        return Ok(());
    }

    let mut registry = lock_runtime_chat_session_registry()?;
    let Some(profile_registry) = registry.get_mut(db_path) else {
        return Ok(());
    };
    let Some(source_entry) = profile_registry.entries.remove(&source_dialog_key) else {
        return Ok(());
    };
    let target_entry = profile_registry
        .entries
        .entry(target_dialog_key)
        .or_default();
    target_entry.is_open_lease |= source_entry.is_open_lease;
    target_entry.auto_reconnect_paused |= source_entry.auto_reconnect_paused;
    target_entry.lease_triggered_at_unix_ms = match (
        target_entry.lease_triggered_at_unix_ms,
        source_entry.lease_triggered_at_unix_ms,
    ) {
        (Some(current), Some(source)) => Some(current.min(source)),
        (Some(current), None) => Some(current),
        (None, Some(source)) => Some(source),
        (None, None) => None,
    };
    target_entry.last_interaction_at_unix_ms = target_entry
        .last_interaction_at_unix_ms
        .max(source_entry.last_interaction_at_unix_ms);
    if source_entry.last_interaction_at_unix_ms >= target_entry.last_interaction_at_unix_ms {
        target_entry.last_interaction_kind = source_entry.last_interaction_kind;
    }
    target_entry.request_signal_observed |= source_entry.request_signal_observed;
    target_entry.request_signal_sent |= source_entry.request_signal_sent;
    target_entry.approval_signal_pending |= source_entry.approval_signal_pending;
    target_entry.last_confirmed_keepalive_at_unix_ms = target_entry
        .last_confirmed_keepalive_at_unix_ms
        .max(source_entry.last_confirmed_keepalive_at_unix_ms);
    target_entry.peer_endpoint = source_entry.peer_endpoint.or(target_entry.peer_endpoint.take());
    target_entry.local_endpoint = source_entry.local_endpoint.or(target_entry.local_endpoint.take());
    Ok(())
}

fn list_open_chat_session_leases(storage: &Storage) -> OmniResult<Vec<OpenChatLeaseSnapshot>> {
    let db_path = active_profile_runtime_chat_session_key(storage);
    let registry = lock_runtime_chat_session_registry()?;
    let Some(profile_registry) = registry.get(&db_path) else {
        return Ok(Vec::new());
    };

    let mut leases = Vec::new();
    for (dialog_key, entry) in &profile_registry.entries {
        if !entry.is_open_lease && !entry.auto_reconnect_paused {
            continue;
        }
        leases.push(OpenChatLeaseSnapshot {
            dialog_key: dialog_key.clone(),
            is_open_lease: entry.is_open_lease,
            auto_reconnect_paused: entry.auto_reconnect_paused,
        });
    }

    Ok(leases)
}

fn select_chat_session_state(
    auto_reconnect_paused: bool,
    has_live_connection: bool,
    is_open_lease: bool,
    session_capable: bool,
    last_confirmed_keepalive_at_unix_ms: Option<i64>,
    now_unix_ms: i64,
) -> omni_types::ChatSessionState {
    let keepalive_is_confirmed = last_confirmed_keepalive_at_unix_ms.is_some();
    let keepalive_is_stale =
        last_confirmed_keepalive_at_unix_ms.is_some_and(|observed_at_unix_ms| {
            now_unix_ms.saturating_sub(observed_at_unix_ms) >= CHAT_SESSION_KEEPALIVE_STALE_AFTER_MS
        });

    if auto_reconnect_paused {
        omni_types::ChatSessionState::Paused
    } else if has_live_connection && keepalive_is_confirmed && !keepalive_is_stale {
        omni_types::ChatSessionState::Connected
    } else if (is_open_lease || has_live_connection) && session_capable {
        omni_types::ChatSessionState::Reconnecting
    } else {
        omni_types::ChatSessionState::Idle
    }
}

/// `link_facts` — то, что ТРАНСПОРТ знает о живом линке (адрес соединения и измеренный RTT).
/// Передаётся снаружи, потому что mesh-рантайм живёт на `OmniApp`, а эта функция свободная.
/// The live link to a contact as the chat panel shows it: what the transport measured, and which of
/// the contact's devices is on the other end.
#[derive(Debug, Clone)]
pub(crate) struct PeerLinkView {
    pub(crate) facts: omni_mesh_runtime::MeshPeerLinkFacts,
    pub(crate) device_id: Option<String>,
    pub(crate) device_count: u32,
}

fn build_chat_session_summary(
    storage: &Storage,
    transport: &TransportManager,
    dialog_key: &str,
    peer_link: Option<PeerLinkView>,
) -> OmniResult<omni_types::ChatSessionSummary> {
    let (peer_device_id, peer_device_count) = peer_link
        .as_ref()
        .map(|view| (view.device_id.clone(), Some(view.device_count)))
        .unwrap_or((None, None));
    let link_facts = peer_link.map(|view| view.facts);
    let canonical_dialog_key = resolve_canonical_dialog_key_for_selector(storage, dialog_key.trim())?;
    let db_path = active_profile_runtime_chat_session_key(storage);
    let registry = lock_runtime_chat_session_registry()?;
    let entry = registry
        .get(&db_path)
        .and_then(|profile_registry| profile_registry.entries.get(&canonical_dialog_key))
        .cloned()
        .unwrap_or_default();
    drop(registry);

    let live_connection = (!entry.auto_reconnect_paused)
        .then(|| transport.connection_snapshot_for_dialog(&canonical_dialog_key))
        .flatten();
    let session_capable = session_capable_dialog(storage, &canonical_dialog_key)?;
    // A LIVE MESH LINK IS A LIVE CONNECTION. The transport registry alone answered "none" for a
    // healthy direct QUIC link, so the panel said «переподключаемся» under a header saying
    // «Онлайн, напрямую · QUIC» (live 2026-10-06: keepalive 9 s old, RTT 51 ms, state
    // reconnecting) — the same blind spot `call_link_health` in app_calls.rs already fixed.
    let has_live_link = live_connection.is_some() || link_facts.is_some();
    let session_state = select_chat_session_state(
        entry.auto_reconnect_paused,
        has_live_link,
        entry.is_open_lease,
        session_capable,
        entry.last_confirmed_keepalive_at_unix_ms,
        current_unix_ms(),
    );

    Ok(omni_types::ChatSessionSummary {
        is_open_lease: entry.is_open_lease,
        auto_reconnect_paused: entry.auto_reconnect_paused,
        session_state,
        // АДРЕС СОЕДИНЕНИЯ, А НЕ ИДЕНТИФИКАТОР УЗЛА. Под mesh сюда приезжал `node_addr` (40 hex),
        // и панель печатала его дважды: «Endpoint» — прямо, «Сеть» — потому что её лестница
        // разборов не узнавала в нём адрес и сваливалась в `return peerEndpoint`.
        peer_endpoint: link_facts
            .and_then(|facts| facts.remote)
            .map(|addr| addr.to_string())
            .or_else(|| {
                live_connection
                    .as_ref()
                    .and_then(|connection| connection.peer_endpoint.clone())
            })
            .or(entry.peer_endpoint),
        local_endpoint: live_connection
            .as_ref()
            .and_then(|connection| connection.local_endpoint.clone())
            .or(entry.local_endpoint),
        last_interaction_at_unix_ms: entry.last_interaction_at_unix_ms,
        last_interaction_kind: entry.last_interaction_kind,
        last_confirmed_keepalive_at_unix_ms: entry.last_confirmed_keepalive_at_unix_ms,
        // ПРОХОД ТУДА-ОБРАТНО МЕРИТ ТРАНСПОРТ, А НЕ МЫ. Здесь стояло значение, посчитанное по
        // presence-обмену: момент отправки вытаскивался из строки идентификатора пинга, и в
        // разницу попадал весь наш конвейер — тик очереди, шифрование, два замка, и то же самое
        // на обратном пути. Живой замер 2026-08-31: presence 322 мс (медиана, n=306) при 49 мс
        // на транспорте и 11 мс на обработку у собеседника.
        last_round_trip_ms: link_facts
            .and_then(|facts| facts.rtt)
            .map(|rtt| rtt.as_millis().min(i64::MAX as u128) as i64),
        peer_device_id,
        peer_device_count,
    })
}

#[cfg(test)]
mod runtime_chat_session_unit_tests {
    use super::*;

    #[test]
    fn live_connection_without_confirmed_keepalive_is_not_connected() {
        assert_eq!(
            select_chat_session_state(false, true, true, true, None, 10_000),
            omni_types::ChatSessionState::Reconnecting
        );
    }

    #[test]
    fn live_connection_with_fresh_confirmed_keepalive_is_connected() {
        assert_eq!(
            select_chat_session_state(false, true, true, true, Some(9_000), 10_000),
            omni_types::ChatSessionState::Connected
        );
    }

    #[test]
    fn live_connection_with_stale_keepalive_is_reconnecting() {
        assert_eq!(
            select_chat_session_state(false, true, true, true, Some(1_000), 50_000),
            omni_types::ChatSessionState::Reconnecting
        );
    }
}
