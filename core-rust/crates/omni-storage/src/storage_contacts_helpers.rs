fn name_looks_like_account_placeholder(value: &str, remote_account_id: Option<&str>) -> bool {
    let trimmed_value = value.trim();
    if trimmed_value.is_empty() {
        return true;
    }

    remote_account_id.is_some_and(|account_id| trimmed_value == account_id.trim())
}

fn current_contact_presence_miss_count(storage: &Storage, contact_id: &ContactId) -> OmniResult<i64> {
    let profile_id = storage.require_active_profile_id()?;
    let contact_id =
        storage.resolve_canonical_contact_id_for_profile(&profile_id, contact_id, current_unix_ms())?;
    Ok(storage
        .connection
        .query_row(
            "
            SELECT consecutive_missed_heartbeats
            FROM contact_presence
            WHERE contact_id = ?1
              AND profile_id = ?2
            ",
            params![contact_id.0, profile_id.0],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(map_sqlite_error)?
        .unwrap_or(0))
}

fn choose_canonical_contact(candidates: &[ContactSnapshot]) -> Option<&ContactSnapshot> {
    candidates
        .iter()
        .max_by_key(|candidate| contact_priority_tuple(candidate))
}

fn merge_contact_snapshots(
    candidates: &[ContactSnapshot],
    now_unix_ms: i64,
) -> Option<ContactSnapshot> {
    let canonical = choose_canonical_contact(candidates)?;
    let remote_account_id = candidates
        .iter()
        .find_map(|candidate| candidate.remote_account_id.clone())
        .or_else(|| canonical.remote_account_id.clone());
    let remote_account_id_hint = remote_account_id.as_ref().map(|value| value.0.as_str());
    let petname = candidates
        .iter()
        .filter(|candidate| !name_looks_like_account_placeholder(&candidate.petname, remote_account_id_hint))
        .max_by_key(|candidate| (candidate.updated_at_unix_ms, candidate.petname.len()))
        .map(|candidate| candidate.petname.clone())
        .unwrap_or_else(|| canonical.petname.clone());

    let profile_candidate = candidates
        .iter()
        .filter(|candidate| {
            candidate.public_display_name.is_some()
                || candidate.public_avatar_hash_hex.is_some()
                || candidate.public_avatar_pending_hash_hex.is_some()
                || candidate.public_profile_updated_at_unix_ms.is_some()
        })
        .max_by_key(|candidate| {
            (
                candidate.public_profile_updated_at_unix_ms.unwrap_or(i64::MIN),
                candidate.updated_at_unix_ms,
            )
        })
        .unwrap_or(canonical);
    let freshest_runtime_candidate = candidates
        .iter()
        .max_by_key(|candidate| candidate.updated_at_unix_ms)
        .unwrap_or(canonical);

    let is_blocked = candidates.iter().any(|candidate| {
        candidate.is_blocked
            || matches!(
                candidate.relationship_state,
                omni_identity::ContactRelationshipState::Declined
            )
    });
    let relationship_state = if is_blocked {
        omni_identity::ContactRelationshipState::Declined
    } else if candidates.iter().any(|candidate| {
        matches!(
            candidate.relationship_state,
            omni_identity::ContactRelationshipState::Approved
        )
    }) {
        omni_identity::ContactRelationshipState::Approved
    } else {
        omni_identity::ContactRelationshipState::Pending
    };
    let created_at_unix_ms = candidates
        .iter()
        .map(|candidate| candidate.created_at_unix_ms)
        .min()
        .unwrap_or(canonical.created_at_unix_ms);
    let updated_at_unix_ms = candidates
        .iter()
        .map(|candidate| candidate.updated_at_unix_ms)
        .max()
        .unwrap_or(canonical.updated_at_unix_ms)
        .max(now_unix_ms);
    let pending_request_direction =
        (relationship_state == omni_identity::ContactRelationshipState::Pending)
            .then(|| {
                candidates.iter().fold(None, |current, candidate| {
                    merge_pending_request_direction(
                        current,
                        candidate.pending_request_direction,
                    )
                })
            })
            .flatten();

    Some(build_contact_snapshot(
        canonical.contact_id.clone(),
        petname,
        remote_account_id,
        profile_candidate.public_display_name.clone(),
        profile_candidate.public_avatar_hash_hex.clone(),
        profile_candidate.public_avatar_mime_type.clone(),
        profile_candidate.public_avatar_pending_hash_hex.clone(),
        profile_candidate.public_avatar_pending_mime_type.clone(),
        canonical.public_avatar_image_path.clone(),
        candidates
            .iter()
            .filter_map(|candidate| candidate.public_profile_updated_at_unix_ms)
            .max()
            .or(profile_candidate.public_profile_updated_at_unix_ms),
        candidates
            .iter()
            .filter(|candidate| candidate.remote_instance_id.is_some())
            .max_by_key(|candidate| candidate.updated_at_unix_ms)
            .and_then(|candidate| candidate.remote_instance_id.clone())
            .or_else(|| canonical.remote_instance_id.clone()),
        canonical.primary_dialog_key.clone(),
        relationship_state,
        pending_request_direction,
        candidates.iter().any(|candidate| candidate.has_met),
        candidates
            .iter()
            .any(|candidate| candidate.friend_relay_consent),
        freshest_runtime_candidate.presence_state,
        candidates
            .iter()
            .filter_map(|candidate| candidate.presence_last_seen_at_unix_ms)
            .max(),
        candidates
            .iter()
            .filter_map(|candidate| candidate.presence_last_sent_at_unix_ms)
            .max(),
        candidates
            .iter()
            .filter_map(|candidate| candidate.presence_last_activity_at_unix_ms)
            .max(),
        None,
        created_at_unix_ms,
        updated_at_unix_ms,
    ))
}

fn merge_pending_request_direction(
    current: Option<ContactRequestDirection>,
    incoming: Option<ContactRequestDirection>,
) -> Option<ContactRequestDirection> {
    match (current, incoming) {
        (Some(ContactRequestDirection::Incoming), _)
        | (_, Some(ContactRequestDirection::Incoming)) => Some(ContactRequestDirection::Incoming),
        (Some(ContactRequestDirection::Outgoing), _)
        | (_, Some(ContactRequestDirection::Outgoing)) => Some(ContactRequestDirection::Outgoing),
        _ => None,
    }
}

fn contact_priority_tuple(candidate: &ContactSnapshot) -> (i32, i32, i32, i32, i64, i64) {
    let relationship_rank = match candidate.relationship_state {
        omni_identity::ContactRelationshipState::Approved => 2,
        omni_identity::ContactRelationshipState::Pending => 1,
        omni_identity::ContactRelationshipState::Declined => 0,
    };
    let has_custom_petname = if name_looks_like_account_placeholder(
        &candidate.petname,
        candidate.remote_account_id.as_ref().map(|value| value.0.as_str()),
    ) {
        0
    } else {
        1
    };
    (
        relationship_rank,
        bool_to_i32(candidate.has_met),
        bool_to_i32(candidate.primary_dialog_key.is_some()),
        has_custom_petname,
        candidate.public_profile_updated_at_unix_ms.unwrap_or(i64::MIN),
        candidate.updated_at_unix_ms,
    )
}

fn bool_to_i32(value: bool) -> i32 {
    if value {
        1
    } else {
        0
    }
}

fn pick_latest_optional_i64(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
}

fn next_contact_heartbeat_interval_ms() -> i64 {
    let mut bytes = [0_u8; 2];
    OsRng.fill_bytes(&mut bytes);
    let sample = u16::from_le_bytes(bytes) as i64;
    10_000_i64 + (sample % 9_001_i64)
}

