impl Storage {
    pub fn upsert_call_session(&self, summary: &CallSummary) -> OmniResult<()> {
        let profile_id = self.resolve_transition_profile_id()?;
        // A call talks to one device of the contact; its history belongs to the contact's chat
        // (docs/decisions/multi-device-equal-nodes.md).
        let chat_dialog_key = self.chat_dialog_key_for(&summary.dialog_key)?;
        self.connection
            .execute(
                "
                INSERT INTO call_sessions (
                    profile_id,
                    call_id,
                    dialog_key,
                    contact_id,
                    direction,
                    state,
                    outcome,
                    decline_reason,
                    local_participant_id,
                    remote_participant_id,
                    initiator_account_id,
                    initiator_device_id,
                    responder_account_id,
                    responder_device_id,
                    local_mic_muted,
                    network_quality,
                    started_at_unix_ms,
                    ringing_started_at_unix_ms,
                    connected_at_unix_ms,
                    ended_at_unix_ms,
                    reconnect_deadline_unix_ms,
                    updated_at_unix_ms,
                    remote_control_json
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)
                ON CONFLICT(call_id) DO UPDATE SET
                    profile_id = excluded.profile_id,
                    dialog_key = excluded.dialog_key,
                    contact_id = excluded.contact_id,
                    direction = excluded.direction,
                    state = excluded.state,
                    outcome = excluded.outcome,
                    decline_reason = excluded.decline_reason,
                    local_participant_id = excluded.local_participant_id,
                    remote_participant_id = excluded.remote_participant_id,
                    initiator_account_id = excluded.initiator_account_id,
                    initiator_device_id = excluded.initiator_device_id,
                    responder_account_id = excluded.responder_account_id,
                    responder_device_id = excluded.responder_device_id,
                    local_mic_muted = excluded.local_mic_muted,
                    network_quality = excluded.network_quality,
                    started_at_unix_ms = excluded.started_at_unix_ms,
                    ringing_started_at_unix_ms = excluded.ringing_started_at_unix_ms,
                    connected_at_unix_ms = excluded.connected_at_unix_ms,
                    ended_at_unix_ms = excluded.ended_at_unix_ms,
                    reconnect_deadline_unix_ms = excluded.reconnect_deadline_unix_ms,
                    updated_at_unix_ms = excluded.updated_at_unix_ms,
                    remote_control_json = excluded.remote_control_json
                ",
                params![
                    profile_id.0,
                    summary.call_id,
                    chat_dialog_key,
                    summary.contact_id,
                    summary.direction.as_db_value(),
                    summary.state.as_db_value(),
                    summary.outcome.map(|value| value.as_db_value()),
                    summary.decline_reason.map(|value| value.as_db_value()),
                    local_participant_id(summary),
                    remote_participant_id(summary),
                    initiator_account_id(summary),
                    initiator_device_id(summary),
                    responder_account_id(summary),
                    responder_device_id(summary),
                    bool_to_sqlite_integer(summary.local_mic_muted),
                    summary.network_quality.map(|value| value.as_db_value()),
                    summary.started_at_unix_ms,
                    summary.ringing_started_at_unix_ms,
                    summary.connected_at_unix_ms,
                    summary.ended_at_unix_ms,
                    summary.reconnect_deadline_unix_ms,
                    summary.updated_at_unix_ms,
                    summary
                        .remote_control
                        .map(|session| serde_json::to_string(&session))
                        .transpose()
                        .map_err(|error| OmniError::Internal(format!("remote control state: {error}")))?,
                ],
            )
            .map_err(map_sqlite_error)?;
        self.replace_call_participants(&summary.call_id, &summary.participants)?;
        Ok(())
    }

    pub fn get_call_summary(&self, call_id: &str) -> OmniResult<Option<CallSummary>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let row = self
            .connection
            .query_row(
                "
                SELECT
                    call_id,
                    dialog_key,
                    contact_id,
                    direction,
                    state,
                    outcome,
                    decline_reason,
                    local_mic_muted,
                    network_quality,
                    started_at_unix_ms,
                    ringing_started_at_unix_ms,
                    connected_at_unix_ms,
                    ended_at_unix_ms,
                    reconnect_deadline_unix_ms,
                    updated_at_unix_ms,
                    remote_control_json
                FROM call_sessions
                WHERE call_id = ?1
                  AND profile_id = ?2
                ",
                params![call_id, profile_id.0],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, i64>(7)? != 0,
                        row.get::<_, Option<String>>(8)?,
                        row.get::<_, i64>(9)?,
                        row.get::<_, Option<i64>>(10)?,
                        row.get::<_, Option<i64>>(11)?,
                        row.get::<_, Option<i64>>(12)?,
                        row.get::<_, Option<i64>>(13)?,
                        row.get::<_, i64>(14)?,
                        row.get::<_, Option<String>>(15)?,
                    ))
                },
            )
            .optional()
            .map_err(map_sqlite_error)?;

        let Some(row) = row else {
            return Ok(None);
        };

        let call_id = row.0;
        Ok(Some(CallSummary {
            call_id: call_id.clone(),
            dialog_key: row.1,
            contact_id: row.2,
            direction: CallDirection::from_db_value(&row.3)?,
            state: CallState::from_db_value(&row.4)?,
            outcome: row
                .5
                .as_deref()
                .map(CallOutcome::from_db_value)
                .transpose()?,
            decline_reason: row
                .6
                .as_deref()
                .map(CallDeclineReason::from_db_value)
                .transpose()?,
            participants: self.list_call_participants(&call_id)?,
            local_mic_muted: row.7,
            network_quality: row
                .8
                .as_deref()
                .map(CallNetworkQuality::from_db_value)
                .transpose()?,
            started_at_unix_ms: row.9,
            ringing_started_at_unix_ms: row.10,
            connected_at_unix_ms: row.11,
            ended_at_unix_ms: row.12,
            reconnect_deadline_unix_ms: row.13,
            updated_at_unix_ms: row.14,
            // Unreadable (a future format): no session rather than a failed call read.
            remote_control: row.15.as_deref().and_then(|json| serde_json::from_str(json).ok()),
        }))
    }

    pub fn get_latest_call_for_dialog(&self, dialog_key: &str) -> OmniResult<Option<CallSummary>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let normalized_dialog_key = normalize_dialog_key(dialog_key);
        let call_id = self
            .connection
            .query_row(
                "
                SELECT call_id
                FROM call_sessions
                WHERE dialog_key = ?1
                  AND profile_id = ?2
                ORDER BY CASE WHEN state = 'ended' THEN 1 ELSE 0 END ASC, updated_at_unix_ms DESC, call_id DESC
                LIMIT 1
                ",
                params![normalized_dialog_key, profile_id.0],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;

        match call_id {
            Some(call_id) => self.get_call_summary(&call_id),
            None => Ok(None),
        }
    }

    /// Force-finalize every non-`ended` call row for this profile as `Failed` (storage only —
    /// no wire event, no live-runtime lookup). The ONLY caller is `OmniApp::get_call_inbox_snapshot`,
    /// and only on the branch where THIS process's live `CALL_RUNTIME_REGISTRY` is empty for the
    /// profile — meaning any non-ended row left in storage cannot possibly belong to a call this
    /// process can still act on (hang up / decline / mute all require the live runtime, which is
    /// gone). Left unfixed, `get_call_inbox_snapshot`'s storage fallback resurrects that stale row
    /// as an apparently-live call (duration counting up forever) on every future poll, and the
    /// user can never end it — hang_up_call/decline_call always fail with "no active call" /
    /// "call id mismatch" since the live registry has nothing to match. This happens whenever the
    /// process is killed/crashes mid-call before `end_runtime_call` can persist state='ended'.
    /// See docs/bugs/orphaned-call-session-unkillable.md.
    pub fn finalize_orphaned_call_sessions(&self, now_unix_ms: i64) -> OmniResult<()> {
        let profile_id = self.resolve_transition_profile_id()?;
        self.connection
            .execute(
                "
                UPDATE call_sessions
                SET state = 'ended',
                    outcome = COALESCE(outcome, 'failed'),
                    ended_at_unix_ms = COALESCE(ended_at_unix_ms, ?2),
                    reconnect_deadline_unix_ms = NULL,
                    updated_at_unix_ms = ?2
                WHERE profile_id = ?1
                  AND state <> 'ended'
                ",
                params![profile_id.0, now_unix_ms],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn get_call_inbox_snapshot(&self) -> OmniResult<CallInboxSnapshot> {
        let profile_id = self.resolve_transition_profile_id()?;
        let current_call_id = self
            .connection
            .query_row(
                "
                SELECT call_id
                FROM call_sessions
                WHERE profile_id = ?1
                  AND state <> 'ended'
                ORDER BY updated_at_unix_ms DESC, call_id DESC
                LIMIT 1
                ",
                params![profile_id.0],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;

        let mut statement = self
            .connection
            .prepare(
                "
                SELECT call_id
                FROM call_sessions
                WHERE profile_id = ?1
                  AND state = 'ringing'
                  AND direction = 'incoming'
                ORDER BY updated_at_unix_ms DESC, call_id DESC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0], |row| row.get::<_, String>(0))
            .map_err(map_sqlite_error)?;

        let mut incoming_calls = Vec::new();
        for row in rows {
            let call_id = row.map_err(map_sqlite_error)?;
            if current_call_id.as_deref() == Some(call_id.as_str()) {
                continue;
            }
            if let Some(summary) = self.get_call_summary(&call_id)? {
                incoming_calls.push(summary);
            }
        }

        Ok(CallInboxSnapshot {
            current_call: match current_call_id {
                Some(call_id) => self.get_call_summary(&call_id)?,
                None => None,
            },
            incoming_calls,
            updated_at_unix_ms: current_unix_ms(),
        })
    }

    pub fn list_call_history_messages_for_dialog(
        &self,
        dialog_key: &str,
    ) -> OmniResult<Vec<ChatMessageRecord>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let normalized_dialog_key = normalize_dialog_key(dialog_key);
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT call_id
                FROM call_sessions
                WHERE dialog_key = ?1
                  AND profile_id = ?2
                  AND state = 'ended'
                  AND outcome IS NOT NULL
                ORDER BY COALESCE(ended_at_unix_ms, updated_at_unix_ms, started_at_unix_ms) ASC, call_id ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![normalized_dialog_key, profile_id.0], |row| {
                row.get::<_, String>(0)
            })
            .map_err(map_sqlite_error)?;

        let mut items = Vec::new();
        for row in rows {
            let call_id = row.map_err(map_sqlite_error)?;
            if let Some(summary) = self.get_call_summary(&call_id)? {
                if let Some(message) = build_call_history_message_record(&summary) {
                    items.push(message);
                }
            }
        }

        Ok(items)
    }

    pub fn get_latest_call_history_message_for_dialog(
        &self,
        dialog_key: &str,
    ) -> OmniResult<Option<ChatMessageRecord>> {
        let mut items = self.list_call_history_messages_for_dialog(dialog_key)?;
        Ok(items.pop())
    }

    pub fn list_latest_call_history_messages_by_dialog(
        &self,
    ) -> OmniResult<std::collections::HashMap<String, ChatMessageRecord>> {
        let profile_id = self.resolve_transition_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT dialog_key, call_id
                FROM (
                    SELECT
                        dialog_key,
                        call_id,
                        ROW_NUMBER() OVER (
                            PARTITION BY dialog_key
                            ORDER BY COALESCE(ended_at_unix_ms, updated_at_unix_ms, started_at_unix_ms) DESC, call_id DESC
                        ) AS row_number
                    FROM call_sessions
                    WHERE profile_id = ?1
                      AND state = 'ended'
                      AND outcome IS NOT NULL
                      AND dialog_key <> ''
                )
                WHERE row_number = 1
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(map_sqlite_error)?;

        let mut items = std::collections::HashMap::new();
        for row in rows {
            let (dialog_key, call_id) = row.map_err(map_sqlite_error)?;
            if let Some(summary) = self.get_call_summary(&call_id)? {
                if let Some(message) = build_call_history_message_record(&summary) {
                    items.insert(dialog_key, message);
                }
            }
        }

        Ok(items)
    }

    fn replace_call_participants(
        &self,
        call_id: &str,
        participants: &[CallParticipantSummary],
    ) -> OmniResult<()> {
        self.connection
            .execute(
                "DELETE FROM call_participants WHERE call_id = ?1",
                params![call_id],
            )
            .map_err(map_sqlite_error)?;

        for participant in participants {
            self.connection
                .execute(
                    "
                    INSERT INTO call_participants (
                        call_id,
                        participant_id,
                        account_id,
                        device_id,
                        display_name,
                        is_local,
                        is_muted
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                    ",
                    params![
                        call_id,
                        participant.participant_id,
                        participant.account_id,
                        participant.device_id,
                        participant.display_name,
                        bool_to_sqlite_integer(participant.is_local),
                        bool_to_sqlite_integer(participant.is_muted),
                    ],
                )
                .map_err(map_sqlite_error)?;
        }

        Ok(())
    }

    fn list_call_participants(&self, call_id: &str) -> OmniResult<Vec<CallParticipantSummary>> {
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT
                    participant_id,
                    account_id,
                    device_id,
                    display_name,
                    is_local,
                    is_muted
                FROM call_participants
                WHERE call_id = ?1
                ORDER BY is_local DESC, participant_id ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![call_id], |row| {
                Ok(CallParticipantSummary {
                    participant_id: row.get(0)?,
                    account_id: row.get(1)?,
                    device_id: row.get(2)?,
                    display_name: row.get(3)?,
                    is_local: row.get::<_, i64>(4)? != 0,
                    is_muted: row.get::<_, i64>(5)? != 0,
                })
            })
            .map_err(map_sqlite_error)?;

        let mut participants = Vec::new();
        for row in rows {
            participants.push(row.map_err(map_sqlite_error)?);
        }
        Ok(participants)
    }
}

fn build_call_history_message_record(summary: &CallSummary) -> Option<ChatMessageRecord> {
    let outcome = summary.outcome?;
    let ended_at_unix_ms = summary.ended_at_unix_ms.unwrap_or(summary.updated_at_unix_ms);
    let duration_seconds = summary.connected_at_unix_ms.and_then(|connected_at_unix_ms| {
        (ended_at_unix_ms >= connected_at_unix_ms)
            .then_some(((ended_at_unix_ms - connected_at_unix_ms) / 1000) as u32)
    });
    Some(ChatMessageRecord {
        id: format!("call-history:{}", summary.call_id),
        dialog_key: summary.dialog_key.clone(),
        direction: match summary.direction {
            CallDirection::Incoming => ChatDirection::Incoming,
            CallDirection::Outgoing => ChatDirection::Outgoing,
        },
        body: String::new(),
        status: MessageStatus::Delivered,
        remote_delivered_at_unix_ms: None,
        created_at_unix_ms: ended_at_unix_ms,
        updated_at_unix_ms: summary.updated_at_unix_ms,
        attachment: None,
        call_history_event: Some(CallHistoryEvent {
            call_id: summary.call_id.clone(),
            direction: summary.direction,
            outcome,
            decline_reason: summary.decline_reason,
            participants: summary.participants.clone(),
            started_at_unix_ms: summary.started_at_unix_ms,
            connected_at_unix_ms: summary.connected_at_unix_ms,
            ended_at_unix_ms: summary.ended_at_unix_ms,
            duration_seconds,
            remote_control: summary.remote_control.is_some(),
        }),
        told_by_contact: false,
    })
}

fn local_participant_id(summary: &CallSummary) -> Option<String> {
    summary
        .participants
        .iter()
        .find(|participant| participant.is_local)
        .map(|participant| participant.participant_id.clone())
}

fn remote_participant_id(summary: &CallSummary) -> Option<String> {
    summary
        .participants
        .iter()
        .find(|participant| !participant.is_local)
        .map(|participant| participant.participant_id.clone())
}

fn initiator_account_id(summary: &CallSummary) -> Option<String> {
    initiator_participant(summary).map(|participant| participant.account_id.clone())
}

fn initiator_device_id(summary: &CallSummary) -> Option<String> {
    initiator_participant(summary).map(|participant| participant.device_id.clone())
}

fn responder_account_id(summary: &CallSummary) -> Option<String> {
    responder_participant(summary).map(|participant| participant.account_id.clone())
}

fn responder_device_id(summary: &CallSummary) -> Option<String> {
    responder_participant(summary).map(|participant| participant.device_id.clone())
}

fn initiator_participant(summary: &CallSummary) -> Option<&CallParticipantSummary> {
    match summary.direction {
        CallDirection::Outgoing => summary.participants.iter().find(|participant| participant.is_local),
        CallDirection::Incoming => summary.participants.iter().find(|participant| !participant.is_local),
    }
}

fn responder_participant(summary: &CallSummary) -> Option<&CallParticipantSummary> {
    match summary.direction {
        CallDirection::Outgoing => summary.participants.iter().find(|participant| !participant.is_local),
        CallDirection::Incoming => summary.participants.iter().find(|participant| participant.is_local),
    }
}
