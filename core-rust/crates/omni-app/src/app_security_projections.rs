impl OmniApp {
    pub fn get_dialog_message_refresh_summary(
        &self,
        dialog_key_or_selector: impl Into<String>,
    ) -> OmniResult<DialogMessageRefreshSummary> {
        let _ = self.ensure_runtime_profile_active("get_dialog_message_refresh_summary")?;
        let dialog_key = resolve_canonical_dialog_key_for_selector(
            &self.storage,
            dialog_key_or_selector.into().trim(),
        )?;
        self.storage.get_dialog_message_refresh_summary(&dialog_key)
    }

    pub fn get_chat_security_summary(
        &self,
        dialog_key_or_selector: impl Into<String>,
    ) -> OmniResult<ChatSecuritySummary> {
        let dialog_key = resolve_canonical_dialog_key_for_selector(
            &self.storage,
            dialog_key_or_selector.into().trim(),
        )?;
        let contact_card = self.storage.resolve_contact_card_for_dialog(&dialog_key)?;
        let remote_account_id = contact_card
            .as_ref()
            .and_then(|card| card.contact.remote_account_id.as_ref().map(|value| value.0.clone()));
        let active_session = match (contact_card.as_ref(), remote_account_id.as_deref()) {
            (Some(card), Some(account)) => {
                chat_security_session(&self.storage, &card.contact.contact_id, &AccountId(account.to_string()), &dialog_key)?
            }
            _ => None,
        };
        let has_remote_bundle = remote_account_id
            .as_deref()
            .map(|value| {
                self.storage
                    .get_latest_remote_prekey_bundle_for_account(&AccountId(value.to_string()))
            })
            .transpose()?
            .flatten()
            .is_some();
        let trust_state = contact_card.as_ref().map(|card| card.trust_state);
        let has_established_session = active_session.as_ref().is_some_and(|session| {
            session.bootstrap_state == omni_crypto::SessionBootstrapState::Established
        });

        Ok(ChatSecuritySummary {
            dialog_key,
            contact_id: contact_card.as_ref().map(|card| card.contact.contact_id.clone()),
            remote_account_id: remote_account_id.clone(),
            remote_device_id: active_session
                .as_ref()
                .map(|session| session.session_address.remote_device_id.0.clone()),
            trust_state,
            warning: warning_from_trust(
                trust_state,
                contact_card
                    .as_ref()
                    .map(|card| {
                        matches!(
                            card.contact.relationship_state,
                            omni_identity::ContactRelationshipState::Declined
                        )
                    })
                    .unwrap_or(false),
                has_established_session,
                has_remote_bundle,
                contact_card.is_some(),
            ),
            has_established_session,
            has_remote_prekey_bundle: has_remote_bundle,
            uses_encrypted_transport: true,
            session_state: active_session.as_ref().map(|session| session.bootstrap_state),
            last_session_update_unix_ms: active_session
                .as_ref()
                .map(|session| session.updated_at_unix_ms),
        })
    }

    pub fn get_message_security_details(
        &self,
        message_id: &str,
    ) -> OmniResult<Option<MessageSecurityDetails>> {
        self.storage.get_message_security_details(message_id)
    }

    pub fn get_message_security_details_batch(
        &self,
        message_ids: &[String],
    ) -> OmniResult<Vec<MessageSecurityDetails>> {
        self.storage.get_message_security_details_batch(message_ids)
    }

    pub fn get_dialog_message_delta(
        &self,
        dialog_key_or_selector: impl Into<String>,
        cursor: &DialogDeltaCursor,
    ) -> OmniResult<DialogDeltaSnapshot> {
        let _ = self.ensure_runtime_profile_active("get_dialog_message_delta")?;
        let dialog_key = resolve_canonical_dialog_key_for_selector(
            &self.storage,
            dialog_key_or_selector.into().trim(),
        )?;
        let refresh_summary = self.storage.get_dialog_message_refresh_summary(&dialog_key)?;
        let next_cursor = DialogDeltaCursor::from_refresh_summary(&refresh_summary);

        let requires_full_reload = cursor.dialog_key != dialog_key
            || (cursor.last_message_updated_at_unix_ms.is_none()
                && refresh_summary.message_count > 0)
            || refresh_summary.message_count < cursor.message_count
            || cursor_is_newer(
                refresh_summary.last_message_updated_at_unix_ms,
                refresh_summary.latest_message_id.as_deref(),
                cursor.last_message_updated_at_unix_ms,
                cursor.latest_message_id.as_deref(),
            )
            || cursor_is_newer(
                refresh_summary.last_attachment_updated_at_unix_ms,
                refresh_summary.last_attachment_message_id.as_deref(),
                cursor.last_attachment_updated_at_unix_ms,
                cursor.last_attachment_message_id.as_deref(),
            )
            || cursor_is_newer(
                refresh_summary.last_security_projection_updated_at_unix_ms,
                refresh_summary.last_security_projection_message_id.as_deref(),
                cursor.last_security_projection_updated_at_unix_ms,
                cursor.last_security_projection_message_id.as_deref(),
            );

        if requires_full_reload {
            return Ok(DialogDeltaSnapshot {
                dialog_key,
                changed_messages: Vec::new(),
                changed_security_details: Vec::new(),
                cursor: next_cursor,
                requires_full_reload: true,
            });
        }

        let changed_messages = self.storage.list_messages_for_dialog_after_cursor(
            &dialog_key,
            cursor.last_message_updated_at_unix_ms,
            cursor.latest_message_id.as_deref(),
            cursor.last_attachment_updated_at_unix_ms,
            cursor.last_attachment_message_id.as_deref(),
        )?;
        let changed_security_details = self.storage.get_message_security_details_delta(
            &dialog_key,
            cursor.last_security_projection_updated_at_unix_ms,
            cursor.last_security_projection_message_id.as_deref(),
        )?;

        Ok(DialogDeltaSnapshot {
            dialog_key,
            changed_messages,
            changed_security_details,
            cursor: next_cursor,
            requires_full_reload: false,
        })
    }
}

/// The session a chat's security summary reports: the chat dialog's device's when established,
/// else any established session with another device of the contact — the chat is secure with the
/// contact if it is secure with one of its devices (docs/decisions/multi-device-equal-nodes.md).
fn chat_security_session(
    storage: &Storage,
    contact_id: &ContactId,
    remote_account_id: &AccountId,
    dialog_key: &str,
) -> OmniResult<Option<omni_crypto::SessionRecord>> {
    let is_established = |session: &omni_crypto::SessionRecord| {
        session.bootstrap_state == omni_crypto::SessionBootstrapState::Established
    };
    let dialog_session = storage.get_session_for_dialog(remote_account_id, dialog_key)?;
    if dialog_session.as_ref().is_some_and(is_established) {
        return Ok(dialog_session);
    }
    for link in storage.list_contact_device_dialogs(contact_id)? {
        if let Some(session) = storage.get_session_for_dialog(remote_account_id, &link.dialog_key)? {
            if is_established(&session) {
                return Ok(Some(session));
            }
        }
    }
    Ok(dialog_session)
}

fn cursor_is_newer(
    latest_updated_at_unix_ms: Option<i64>,
    latest_id: Option<&str>,
    cursor_updated_at_unix_ms: Option<i64>,
    cursor_id: Option<&str>,
) -> bool {
    match (latest_updated_at_unix_ms, cursor_updated_at_unix_ms) {
        (Some(latest), Some(cursor)) if latest < cursor => true,
        (Some(latest), Some(cursor)) if latest == cursor => match (latest_id, cursor_id) {
            (Some(latest_id), Some(cursor_id)) => latest_id < cursor_id,
            (None, Some(_)) => true,
            _ => false,
        },
        (None, Some(_)) => true,
        _ => false,
    }
}
