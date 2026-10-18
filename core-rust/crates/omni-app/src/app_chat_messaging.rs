impl OmniApp {
    pub fn send_text(
        &self,
        peer_addr_or_key: impl Into<String>,
        body: impl Into<String>,
    ) -> OmniResult<ChatMessageRecord> {
        let _ = self.ensure_runtime_profile_active("send_message")?;

        let request = SendTextRequest {
            dialog_key: peer_addr_or_key.into(),
            body: body.into(),
        };

        let peer_selector = request.dialog_key.trim();
        let body = request.body.trim();

        if peer_selector.is_empty() {
            return Err(OmniError::InvalidInput(
                "dialog_key must not be empty".to_string(),
            ));
        }

        if body.is_empty() {
            return Err(OmniError::InvalidInput(
                "body must not be empty".to_string(),
            ));
        }

        // Reject an over-cap body up front. The receiver enforces the same
        // ceiling in `validate_chat_text_v1` (CHAT_BODY_MAX_BYTES); without this
        // send-side guard the sender's send_text returns OK, the message is
        // queued and "sent", then the peer silently rejects the decrypted
        // envelope on validation — the message vanishes with no error to the
        // caller (observed identically over both carriers: direct UDP and the
        // TCP door). Fail closed here so the UI/caller learns immediately and
        // can route large content through an attachment instead. The check
        // mirrors the receiver exactly: same trimmed body, same byte length,
        // same `> max` boundary.
        if body.len() > omni_protocol::CHAT_BODY_MAX_BYTES {
            return Err(OmniError::InvalidInput(format!(
                "message body exceeds the {} byte limit; send large content as an attachment instead",
                omni_protocol::CHAT_BODY_MAX_BYTES
            )));
        }

        let requested_dialog_key =
            resolve_canonical_dialog_key_for_selector(&self.storage, peer_selector)?;
        let active_snapshot = self.transport.snapshot();
        let selector_matches_active = selector_matches_snapshot(peer_selector, &active_snapshot)
            || active_snapshot.dialog_key.as_deref() == Some(requested_dialog_key.as_str());

        let dialog_key = active_snapshot
            .dialog_key
            .clone()
            .filter(|_| selector_matches_active)
            .unwrap_or_else(|| requested_dialog_key.clone());

        if let Some(contact_card) = self.storage.resolve_contact_card_for_dialog(&dialog_key)? {
            if contact_card.contact.relationship_state
                != omni_identity::ContactRelationshipState::Approved
            {
                return Err(OmniError::InvalidInput(
                    "contact must be approved before sending messages".to_string(),
                ));
            }
        } else {
            return Err(OmniError::InvalidInput(
                "contact must be approved before sending messages".to_string(),
            ));
        }

        let dialog_key = resume_chat_session_lease_for_profile(&self.storage, &dialog_key)?;
        let _ = touch_runtime_chat_session_interaction(
            &self.storage.db_path().to_string_lossy(),
            &dialog_key,
            omni_types::ChatSessionInteractionKind::Message,
            current_unix_ms(),
        );

        let message_id = next_message_id();
        let created_at_unix_ms = current_unix_ms();

        if selector_matches_active {
            let _session_guard = lock_app_session_guard(&self.session_lock)?;
            self.storage.insert_outgoing_message(
                &message_id,
                &dialog_key,
                body,
                MessageStatus::Queued,
                created_at_unix_ms,
            )?;
            attempt_send_existing_message(&self.storage, &self.transport, &message_id)
        } else {
            self.storage.insert_outgoing_message(
                &message_id,
                &dialog_key,
                body,
                MessageStatus::Queued,
                created_at_unix_ms,
            )?;
            // The target is not the active transport dialog. If it has a known mesh
            // route, attempt delivery now anyway: a live link sends directly, and an
            // offline peer with a route hands the message to durable store-and-forward
            // (the carrier returns HeldDurable → the message is marked sent, so it is
            // not retried and exactly one frame is held). Without a mesh route there is
            // nothing to attempt — fall through to the queue (legacy behaviour).
            let has_mesh_route = self
                .mesh_dialogs
                .lock()
                .ok()
                .and_then(|map| map.node_for_dialog(&dialog_key))
                .is_some();
            if has_mesh_route {
                let _session_guard = lock_app_session_guard(&self.session_lock)?;
                attempt_send_existing_message(&self.storage, &self.transport, &message_id)
            } else {
                self.storage.get_message(&message_id)
            }
        }
    }

    pub fn list_messages(
        &self,
        peer_addr_or_key: impl Into<String>,
    ) -> OmniResult<ListMessagesResponse> {
        let peer_addr_or_key = peer_addr_or_key.into();
        let trimmed = peer_addr_or_key.trim();

        if trimmed.is_empty() {
            return Err(OmniError::InvalidInput(
                "dialog_key must not be empty".to_string(),
            ));
        }

        let dialog_key = resolve_canonical_dialog_key_for_selector(&self.storage, trimmed)?;
        Ok(ListMessagesResponse {
            dialog_key: dialog_key.clone(),
            messages: self.storage.list_messages_for_dialog(&dialog_key)?,
        })
    }

    pub fn list_messages_page(
        &self,
        request: ListMessagesPageRequest,
    ) -> OmniResult<ListMessagesPageResponse> {
        let trimmed = request.dialog_key.trim();

        if trimmed.is_empty() {
            return Err(OmniError::InvalidInput(
                "dialog_key must not be empty".to_string(),
            ));
        }

        if request.before_created_at_unix_ms.is_some() != request.before_message_id.is_some() {
            return Err(OmniError::InvalidInput(
                "before cursor must include both before_created_at_unix_ms and before_message_id"
                    .to_string(),
            ));
        }

        if request.limit == 0 {
            return Err(OmniError::InvalidInput(
                "limit must be greater than zero".to_string(),
            ));
        }

        let dialog_key = resolve_canonical_dialog_key_for_selector(&self.storage, trimmed)?;
        self.storage.list_messages_page_for_dialog(
            &dialog_key,
            request.before_created_at_unix_ms,
            request.before_message_id.as_deref(),
            request.limit,
        )
    }

    pub fn send_attachment(
        &self,
        peer_addr_or_key: impl Into<String>,
        body: impl Into<String>,
        source_file_path: impl Into<String>,
        display_file_name: Option<String>,
        media_protection_mode: omni_types::AttachmentMediaProtectionMode,
    ) -> OmniResult<ChatMessageRecord> {
        self.send_attachment_request(omni_types::SendAttachmentRequest {
            dialog_key: peer_addr_or_key.into(),
            body: body.into(),
            source_file_path: source_file_path.into(),
            display_file_name,
            media_protection_mode,
            as_photo: false,
        })
    }

    /// A file, or with `as_photo` a picture re-encoded and shown in the chat
    /// (docs/decisions/media-messages.md §1).
    pub fn send_attachment_request(
        &self,
        request: omni_types::SendAttachmentRequest,
    ) -> OmniResult<ChatMessageRecord> {
        let _ = self.ensure_runtime_profile_active("send_attachment")?;

        let peer_selector = request.dialog_key.trim();
        let source_file_path = request.source_file_path.trim();
        let body = request.body.trim();

        if peer_selector.is_empty() {
            return Err(OmniError::InvalidInput(
                "dialog_key must not be empty".to_string(),
            ));
        }
        if source_file_path.is_empty() {
            return Err(OmniError::InvalidInput(
                "source_file_path must not be empty".to_string(),
            ));
        }

        let requested_dialog_key =
            resolve_canonical_dialog_key_for_selector(&self.storage, peer_selector)?;
        let active_snapshot = self.transport.snapshot();
        let selector_matches_active = selector_matches_snapshot(peer_selector, &active_snapshot)
            || active_snapshot.dialog_key.as_deref() == Some(requested_dialog_key.as_str());

        let dialog_key = active_snapshot
            .dialog_key
            .clone()
            .filter(|_| selector_matches_active)
            .unwrap_or_else(|| requested_dialog_key.clone());

        if let Some(contact_card) = self.storage.resolve_contact_card_for_dialog(&dialog_key)? {
            if contact_card.contact.relationship_state
                != omni_identity::ContactRelationshipState::Approved
            {
                return Err(OmniError::InvalidInput(
                    "contact must be approved before sending attachments".to_string(),
                ));
            }
        } else {
            return Err(OmniError::InvalidInput(
                "contact must be approved before sending attachments".to_string(),
            ));
        }

        let dialog_key = resume_chat_session_lease_for_profile(&self.storage, &dialog_key)?;
        let _ = touch_runtime_chat_session_interaction(
            &self.storage.db_path().to_string_lossy(),
            &dialog_key,
            omni_types::ChatSessionInteractionKind::Message,
            current_unix_ms(),
        );

        let message_id = next_message_id();
        // A picture this app cannot read (or one over the limits) fails here, before anything is
        // queued.
        let staged_photo =
            stage_outgoing_photo_if_asked(&self.storage, &message_id, source_file_path, request.as_photo)?;
        let created_at_unix_ms = current_unix_ms();
        // The file first, the message second. The queue (another thread) sends whatever message is
        // queued, as a file offer if it has a file row and as plain text if not; with the message
        // written first, it took the message while the file was still being hashed and sent it as
        // text — the receiver got the words and never the file (live 2026-10-15: a 108 MB build,
        // sent 150 ms after the message was queued; the bigger the file, the surer). A file row with
        // no message (the insert below failed) is never sent nor shown.
        register_outgoing_attachment(
            &self.storage,
            &message_id,
            &dialog_key,
            source_file_path,
            request.display_file_name.as_deref(),
            staged_photo,
            request.media_protection_mode,
            created_at_unix_ms,
        )?;
        self.storage.insert_outgoing_message(
            &message_id,
            &dialog_key,
            body,
            MessageStatus::Queued,
            created_at_unix_ms,
        )?;

        if selector_matches_active {
            let _session_guard = lock_app_session_guard(&self.session_lock)?;
            attempt_send_existing_message(&self.storage, &self.transport, &message_id)
        } else {
            self.storage.get_message(&message_id)
        }
    }

    pub fn request_attachment_download(
        &self,
        message_id: impl Into<String>,
        save_path: impl Into<String>,
    ) -> OmniResult<ChatMessageRecord> {
        let _ = self.ensure_runtime_profile_active("request_attachment_download")?;
        let message_id = message_id.into();
        let save_path = save_path.into();
        let trimmed_message_id = message_id.trim();
        let trimmed_save_path = save_path.trim();

        if trimmed_message_id.is_empty() {
            return Err(OmniError::InvalidInput(
                "message_id must not be empty".to_string(),
            ));
        }
        if trimmed_save_path.is_empty() {
            return Err(OmniError::InvalidInput(
                "save_path must not be empty".to_string(),
            ));
        }

        // TM-006 follow-up (2026-05-21): validate the save_path at the
        // entry so the post-download `fs::remove_file` +
        // `fs::rename` in
        // `complete_attachment_download_from_temp_with_verified_sha256`
        // cannot land at a protected system location. The local-blob
        // branch below already routes through `save_attachment_copy_to_path`
        // which is confined, but the download-branch persists
        // `requested_save_path` to DB and consumes it later — confining
        // here prevents the invalid path from ever entering DB state.
        // See docs/decisions/tm-006-destructive-path-confinement.md.
        Storage::confine_destructive_path_for_callers(Path::new(trimmed_save_path))?;

        let message = self.storage.get_message(trimmed_message_id)?;
        if message.dialog_key.starts_with("group:") {
            // A group's file: a member asks for it (membership is its own axis, not a contact —
            // group-trust-model.md), and the mesh tick pulls it from the members who hold it.
            let member = self
                .storage
                .list_groups()?
                .iter()
                .any(|group| group.dialog_key == message.dialog_key && group.my_status == "member");
            if !member {
                return Err(OmniError::InvalidInput(
                    "only a member of the group can download its files".to_string(),
                ));
            }
        } else {
            let contact_card = self
                .storage
                .resolve_contact_card_for_dialog(&message.dialog_key)?
                .ok_or_else(|| {
                    OmniError::InvalidInput(
                        "contact must be approved before downloading attachments".to_string(),
                    )
                })?;
            if contact_card.contact.relationship_state != omni_identity::ContactRelationshipState::Approved {
                return Err(OmniError::InvalidInput(
                    "contact must be approved before downloading attachments".to_string(),
                ));
            }
        }

        let runtime = self
            .storage
            .get_attachment_runtime_info(trimmed_message_id)?
            .ok_or_else(|| {
                OmniError::InvalidInput("attachment message was not found".to_string())
            })?;

        if runtime.attachment.local_blob_available {
            self.storage.save_attachment_copy_to_path(
                trimmed_message_id,
                Path::new(trimmed_save_path),
                current_unix_ms(),
            )?;
            return self.storage.get_message(trimmed_message_id);
        }

        let transfer_id = next_attachment_transfer_id()?;
        self.storage.mark_attachment_download_requested(
            trimmed_message_id,
            Path::new(trimmed_save_path),
            &transfer_id,
            current_unix_ms(),
        )?;

        // Ask right away whichever dialog is the active transport dialog: with several linked
        // devices the chat's dialog is usually not it, and gating on that left the request unsent.
        // A failed attempt leaves the row `requested`; the queue tick retries it.
        {
            let _session_guard = lock_app_session_guard(&self.session_lock)?;
            let _ = attempt_send_attachment_download_request(
                &self.storage,
                &self.transport,
                trimmed_message_id,
            );
        }

        self.storage.get_message(trimmed_message_id)
    }

    pub fn revoke_attachment_access(
        &self,
        message_id: impl Into<String>,
    ) -> OmniResult<ChatMessageRecord> {
        let _ = self.ensure_runtime_profile_active("revoke_attachment_access")?;
        let message_id = message_id.into();
        let trimmed_message_id = message_id.trim();
        if trimmed_message_id.is_empty() {
            return Err(OmniError::InvalidInput(
                "message_id must not be empty".to_string(),
            ));
        }

        let message = self.storage.get_message(trimmed_message_id)?;
        // Still being prepared: nothing was offered, there is no one to tell — the preparation is
        // cancelled (its pass stops at its next progress mark) and the message fails.
        if self
            .storage
            .fail_outgoing_attachment_preparation(trimmed_message_id, "cancelled", current_unix_ms())?
        {
            return self.storage.get_message(trimmed_message_id);
        }
        self.storage
            .revoke_local_attachment_access(trimmed_message_id, current_unix_ms())?;
        if let Some(contact) = self.storage.resolve_contact_for_dialog(&message.dialog_key)? {
            if let Some(remote_account_id) = contact.remote_account_id {
                let payload = omni_types::WirePayloadV1::LogicalEvent(
                    omni_types::LogicalEventEnvelopeV1::attachment_access_revoked(
                        trimmed_message_id,
                        current_unix_ms(),
                    ),
                );
                if let Ok((mut outgoing, _bootstrap_sent_at)) = prepare_encrypted_payload_for_remote_account(
                    &self.storage,
                    &self.transport,
                    &remote_account_id,
                    Some(&message.dialog_key),
                    &payload,
                    SecurityPayloadKind::AttachmentAccessRevoked,
                ) {
                    if self.storage.store_session_record(&mut outgoing.updated_session).is_ok() {
                        let _ = self
                            .transport
                            .send_wire_envelope_for_dialog_key(&message.dialog_key, &outgoing.envelope);
                    }
                }
            }
        }

        self.storage.get_message(trimmed_message_id)
    }
}

impl Drop for OmniApp {
    fn drop(&mut self) {
        // The mesh first: its autoconnect worker and helpers hold their own handles and would
        // otherwise keep dialing for an app that no longer exists.
        self.stop_mesh();
        self.queue_runtime.shutdown();
        let _ = self.transport.disconnect_and_reset();
        if let Ok(mut transport_handle) = self.transport_handle.lock() {
            transport_handle.take();
        }
    }
}

/// Counter incremented every time `lock_app_session_guard` or `lock_session`
/// recovers from a poisoned `session_lock`. Exposed via
/// `session_lock_poison_recovery_count()` for future diagnostics endpoints.
///
/// Rationale: see `docs/decisions/session-lock-recovery.md`. The lock is
/// `Mutex<()>` (no data), and SQLite handles transactional rollback on panic,
/// so recovery is safe. The counter gives visibility without depending on a
/// logging framework (Omni-core does not currently have one).
static SESSION_LOCK_POISON_RECOVERIES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[allow(dead_code)] // exposed for future diagnostics FFI endpoint and tests
pub(crate) fn session_lock_poison_recovery_count() -> u64 {
    SESSION_LOCK_POISON_RECOVERIES.load(std::sync::atomic::Ordering::Relaxed)
}

/// The session as it is stored NOW. Call it after taking the session lock, before encrypting with a
/// session that was loaded earlier — e.g. while waiting for it to become established. Encrypting
/// advances the ratchet and the result is written back whole, so a copy loaded before the lock
/// rolls back everything the receive path did in between: the peers' ratchets diverge for good and
/// every 1:1 frame after that fails to decrypt
/// (docs/bugs/stale-session-copy-diverges-ratchet.md). `None` if the session is gone or no longer
/// established.
pub(crate) fn reload_established_session_under_lock(
    storage: &Storage,
    loaded_earlier: &omni_crypto::SessionRecord,
) -> OmniResult<Option<omni_crypto::SessionRecord>> {
    Ok(storage
        .get_session_record(&loaded_earlier.session_address)?
        .filter(|session| {
            session.bootstrap_state == omni_crypto::SessionBootstrapState::Established
        }))
}

fn lock_app_session_guard<'a>(
    session_lock: &'a Arc<Mutex<()>>,
) -> OmniResult<std::sync::MutexGuard<'a, ()>> {
    // Poison recovery: `session_lock` is a `Mutex<()>` (no inner data), and
    // crypto/storage operations under it are protected by SQLite transactional
    // atomicity. A panic inside the guarded section may have left SQLite to
    // roll back, but the lock itself has nothing to corrupt. Recovering keeps
    // the queue worker and user-flow alive instead of silently deadlocking.
    // See docs/decisions/session-lock-recovery.md.
    Ok(session_lock.lock().unwrap_or_else(|poison| {
        SESSION_LOCK_POISON_RECOVERIES
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        poison.into_inner()
    }))
}
