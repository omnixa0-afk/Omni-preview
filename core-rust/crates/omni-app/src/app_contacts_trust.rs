#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContactsScreenSnapshot {
    pub refresh_summary: omni_types::ContactsRefreshSummary,
    pub contacts: Vec<ContactSnapshot>,
}

impl OmniApp {
    fn active_transport_matches_contact(&self, contact: &ContactSnapshot) -> bool {
        let active_remote_account_id = self.transport.active_remote_account_id();
        let active_remote_instance_id = self.transport.active_remote_instance_id();

        let matches_active_account = active_remote_account_id
            .as_deref()
            .zip(contact.remote_account_id.as_ref())
            .is_some_and(|(active, known)| known.0 == active);
        let matches_active_instance = active_remote_instance_id
            .as_deref()
            .zip(contact.remote_instance_id.as_deref())
            .is_some_and(|(active, known)| known == active);

        matches_active_account || matches_active_instance
    }

    fn active_transport_session(&self) -> OmniResult<Option<omni_crypto::SessionRecord>> {
        if let Some(bundle) = self.transport.active_remote_prekey_bundle() {
            let exact_session = self.storage.get_session_record(&omni_crypto::SessionAddress {
                remote_account_id: omni_identity::AccountId(bundle.account.account_id.clone()),
                remote_device_id: omni_identity::DeviceId(bundle.device.device_id.clone()),
            })?;
            if exact_session.is_some() {
                return Ok(exact_session);
            }
        }

        let Some(remote_account_id) = self
            .transport
            .active_remote_account_id()
            .map(omni_identity::AccountId)
        else {
            return Ok(None);
        };
        match self.transport.active_dialog_key() {
            Some(dialog_key) => self
                .storage
                .get_session_for_dialog(&remote_account_id, &dialog_key),
            None => self
                .storage
                .get_latest_session_for_remote_account(&remote_account_id),
        }
    }

    /// The session for a signal to `contact`. Signals go out on the active transport's dialog when
    /// it is this contact's, else on the contact's own dialog — so the session is that dialog's
    /// device's: with several devices the account's latest session may be another device's, which
    /// cannot decrypt what arrives on this dialog (docs/decisions/multi-device-equal-nodes.md).
    /// An established session wins over one still bootstrapping.
    fn session_for_contact_on_active_transport(
        &self,
        contact: &ContactSnapshot,
    ) -> OmniResult<Option<omni_crypto::SessionRecord>> {
        let active_session = if self.active_transport_matches_contact(contact) {
            self.active_transport_session()?
        } else {
            None
        };
        let contact_session = match contact.remote_account_id.as_ref() {
            Some(remote_account_id) => match contact.primary_dialog_key.as_deref() {
                Some(dialog_key) => self
                    .storage
                    .get_session_for_dialog(remote_account_id, dialog_key)?,
                None => self
                    .storage
                    .get_latest_session_for_remote_account(remote_account_id)?,
            },
            None => None,
        };
        let is_established = |session: &Option<omni_crypto::SessionRecord>| {
            session.as_ref().is_some_and(|session| {
                session.bootstrap_state == omni_crypto::SessionBootstrapState::Established
            })
        };
        Ok(if is_established(&active_session) {
            active_session
        } else if is_established(&contact_session) {
            contact_session
        } else {
            active_session.or(contact_session)
        })
    }

    fn wait_briefly_for_established_contact_session_on_active_transport(
        &self,
        contact: &ContactSnapshot,
    ) -> OmniResult<Option<omni_crypto::SessionRecord>> {
        let mut session = self
            .session_for_contact_on_active_transport(contact)?
            .filter(|value| {
                value.bootstrap_state == omni_crypto::SessionBootstrapState::Established
            });
        if session.is_some() {
            return Ok(session);
        }

        #[cfg(not(test))]
        let attempts = 24;
        #[cfg(test)]
        let attempts = 32;
        #[cfg(not(test))]
        let delay_ms = 50;
        #[cfg(test)]
        let delay_ms = 15;

        for _ in 0..attempts {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            session = self
                .session_for_contact_on_active_transport(contact)?
                .filter(|value| {
                    value.bootstrap_state == omni_crypto::SessionBootstrapState::Established
                });
            if session.is_some() {
                break;
            }
        }

        Ok(session)
    }

    fn wait_briefly_for_contact_request_signal_on_active_transport(
        &self,
        dialog_key: Option<&str>,
    ) -> OmniResult<bool> {
        let Some(dialog_key) = dialog_key else {
            return Ok(false);
        };
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        if runtime_chat_session_request_signal_observed(&db_path, dialog_key)? {
            return Ok(true);
        }

        #[cfg(not(test))]
        let attempts = 24;
        #[cfg(test)]
        let attempts = 32;
        #[cfg(not(test))]
        let delay_ms = 50;
        #[cfg(test)]
        let delay_ms = 15;

        for _ in 0..attempts {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            if runtime_chat_session_request_signal_observed(&db_path, dialog_key)? {
                return Ok(true);
            }
        }

        Ok(false)
    }

    fn maybe_bind_contact_to_active_transport(
        &self,
        contact: &ContactSnapshot,
        now_unix_ms: i64,
    ) -> OmniResult<(ContactSnapshot, bool)> {
        let local_account_id = self
            .storage
            .get_local_account()?
            .map(|account| account.descriptor.account_id);
        let local_instance_id = self.storage.get_or_create_local_instance_id()?;
        let active_remote_account_id = self.transport.active_remote_account_id();
        let active_remote_instance_id = self.transport.active_remote_instance_id();
        let active_dialog_key = self.transport.active_dialog_key();
        let contact_uses_local_identity = contact
            .remote_account_id
            .as_ref()
            .zip(local_account_id.as_ref())
            .is_some_and(|(remote, local)| remote == local)
            || contact
                .remote_instance_id
                .as_deref()
                .map(str::trim)
                .is_some_and(|remote| remote == local_instance_id);
        let active_transport_uses_local_identity = active_remote_account_id
            .as_deref()
            .zip(local_account_id.as_ref())
            .is_some_and(|(remote, local)| remote == local.0.as_str())
            || active_remote_instance_id
                .as_deref()
                .is_some_and(|remote| remote == local_instance_id);
        if contact_uses_local_identity || active_transport_uses_local_identity {
            return Ok((contact.clone(), false));
        }
        let matches_active_transport = self.active_transport_matches_contact(contact);

        if !matches_active_transport {
            return Ok((contact.clone(), false));
        }

        let bound = self.storage.add_contact(
            &AddContactInput {
                petname: contact.petname.clone(),
                remote_account_id: contact
                    .remote_account_id
                    .clone()
                    .or_else(|| active_remote_account_id.clone().map(omni_identity::AccountId)),
                public_display_name: contact
                    .public_display_name
                    .clone()
                    .or_else(|| active_remote_account_id.clone()),
                remote_instance_id: contact
                    .remote_instance_id
                    .clone()
                    .or(active_remote_instance_id),
                primary_dialog_key: contact.primary_dialog_key.clone().or(active_dialog_key),
                initial_observation: None,
            },
            now_unix_ms,
        )?;

        Ok((bound, true))
    }

    pub fn add_contact_by_identifier(
        &self,
        account_id: impl Into<String>,
        petname: impl Into<String>,
        public_display_name: Option<String>,
    ) -> OmniResult<ContactSnapshot> {
        let _ = self.ensure_runtime_profile_active("add_contact_by_identifier")?;
        let account_id = account_id.into();
        let account_id = account_id.trim();

        if account_id.is_empty() {
            return Err(OmniError::InvalidInput(
                "account_id must not be empty".to_string(),
            ));
        }

        let public_display_name = public_display_name
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| account_id.to_string());

        self.add_contact(&AddContactInput {
            petname: petname.into(),
            remote_account_id: Some(omni_identity::AccountId(account_id.to_string())),
            public_display_name: Some(public_display_name),
            remote_instance_id: None,
            primary_dialog_key: None,
            initial_observation: None,
        })
    }

    pub fn request_contact_by_identifier(
        &self,
        account_id: impl Into<String>,
        petname: impl Into<String>,
        public_display_name: Option<String>,
    ) -> OmniResult<ContactSnapshot> {
        let _ = self.ensure_runtime_profile_active("request_contact_by_identifier")?;
        let account_id = account_id.into();
        let account_id = account_id.trim();

        if account_id.is_empty() {
            return Err(OmniError::InvalidInput(
                "account_id must not be empty".to_string(),
            ));
        }

        let public_display_name = public_display_name
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| account_id.to_string());

        self.storage.add_outgoing_pending_contact(
            &AddContactInput {
                petname: petname.into(),
                remote_account_id: Some(omni_identity::AccountId(account_id.to_string())),
                public_display_name: Some(public_display_name),
                remote_instance_id: None,
                primary_dialog_key: None,
                initial_observation: None,
            },
            current_unix_ms(),
        )
    }

    pub fn add_contact(&self, input: &AddContactInput) -> OmniResult<ContactSnapshot> {
        let _ = self.ensure_runtime_profile_active("add_contact")?;
        let added = self.storage.add_contact(input, current_unix_ms())?;
        self.push_contact_change_to_own_devices();
        Ok(added)
    }

    pub fn rename_contact(
        &self,
        contact_id: &ContactId,
        petname: impl Into<String>,
    ) -> OmniResult<ContactSnapshot> {
        let _ = self.ensure_runtime_profile_active("rename_contact")?;
        let renamed = self
            .storage
            .rename_contact(contact_id, &petname.into(), current_unix_ms())?;
        self.push_contact_change_to_own_devices();
        Ok(renamed)
    }

    pub fn block_contact(&self, contact_id: &ContactId) -> OmniResult<ContactSnapshot> {
        let _ = self.ensure_runtime_profile_active("block_contact")?;
        let blocked = self.storage.decline_contact(contact_id, current_unix_ms())?;
        self.push_contact_change_to_own_devices();
        Ok(blocked)
    }

    pub fn approve_contact(&self, contact_id: &ContactId) -> OmniResult<ContactSnapshot> {
        let _ = self.ensure_runtime_profile_active("approve_contact")?;
        let now_unix_ms = current_unix_ms();
        let previous_contact = self.storage.get_contact(contact_id)?;
        if previous_contact.relationship_state == ContactRelationshipState::Pending
            && previous_contact.pending_request_direction != Some(ContactRequestDirection::Incoming)
        {
            return Err(OmniError::InvalidInput(
                "outgoing contact request is waiting for remote approval".to_string(),
            ));
        }
        let approved = self.storage.approve_contact(contact_id, now_unix_ms)?;
        let (mut approved, matched_active_transport) =
            self.maybe_bind_contact_to_active_transport(&approved, now_unix_ms)?;
        let latest_session = if matched_active_transport {
            self.session_for_contact_on_active_transport(&approved)?
        } else {
            None
        };
        let has_established_session = latest_session.as_ref().is_some_and(|session| {
            session.bootstrap_state == omni_crypto::SessionBootstrapState::Established
        });
        omni_app_trace_log(
            "approve/gate",
            &format!(
                "matched_active_transport={} has_established_session={} prev_state={:?} prev_dir={:?}",
                matched_active_transport,
                has_established_session,
                previous_contact.relationship_state,
                previous_contact.pending_request_direction
            ),
        );
        if matched_active_transport && has_established_session && !approved.has_met {
            approved = self
                .storage
                .mark_contact_met(&approved.contact_id, now_unix_ms)?;
        }
        if matched_active_transport {
            let snapshot = self.transport.snapshot();
            let target_dialog_key = snapshot.dialog_key.as_deref();
            if let Some(dialog_key) = target_dialog_key {
                let db_path = self.storage.db_path().to_string_lossy().to_string();
                let _ = resume_chat_session_lease_for_profile(&self.storage, dialog_key);
                if let Some(connection) = self.transport.connection_snapshot_for_dialog(dialog_key) {
                    let _ = set_runtime_chat_session_endpoints(
                        &db_path,
                        dialog_key,
                        connection.local_endpoint.as_deref(),
                        connection.peer_endpoint.as_deref(),
                    );
                }
                let _ = touch_runtime_chat_session_interaction(
                    &db_path,
                    dialog_key,
                    omni_types::ChatSessionInteractionKind::Approval,
                    now_unix_ms,
                );
            }
            let request_signal_confirmed =
                previous_contact.relationship_state != ContactRelationshipState::Pending
                    || previous_contact.pending_request_direction
                        == Some(ContactRequestDirection::Incoming)
                    || self.wait_briefly_for_contact_request_signal_on_active_transport(
                        target_dialog_key,
                    )?;
            let should_send_approval_signal =
                previous_contact.relationship_state != ContactRelationshipState::Approved
                    && request_signal_confirmed;
            omni_app_trace_log(
                "approve/gate2",
                &format!(
                    "request_signal_confirmed={} should_send_approval_signal={} target_dialog_key={:?}",
                    request_signal_confirmed, should_send_approval_signal, target_dialog_key
                ),
            );
            if should_send_approval_signal {
                if let Some(dialog_key) = target_dialog_key {
                    let db_path = self.storage.db_path().to_string_lossy().to_string();
                    let _ = mark_runtime_chat_session_approval_signal_pending(&db_path, dialog_key);
                }
            }
            if previous_contact.relationship_state != ContactRelationshipState::Approved {
                if let (Some(dialog_key), Some(peer_id)) =
                    (snapshot.dialog_key.as_deref(), snapshot.peer_id.as_deref())
                {
                    let db_path = self.storage.db_path().to_string_lossy().to_string();
                    mark_verification_challenge_ready(&db_path, dialog_key, peer_id);
                }
            }
            if has_established_session {
                if should_send_approval_signal {
                    let _session_guard = lock_app_session_guard(&self.session_lock)?;
                    if let Some(session) = self
                        .session_for_contact_on_active_transport(&approved)?
                        .filter(|value| {
                            value.bootstrap_state
                                == omni_crypto::SessionBootstrapState::Established
                        })
                    {
                        let _ = send_contact_approval_signal_for_session(
                            &self.storage,
                            &self.transport,
                            &session,
                            target_dialog_key,
                        );
                    }
                }
            } else {
                let waited_session = if previous_contact.relationship_state
                    == ContactRelationshipState::Pending
                {
                    self.wait_briefly_for_established_contact_session_on_active_transport(&approved)?
                } else {
                    None
                };
                if let Some(session) = waited_session.as_ref() {
                    if !approved.has_met
                        && session.bootstrap_state
                            == omni_crypto::SessionBootstrapState::Established
                    {
                        approved = self
                            .storage
                            .mark_contact_met(&approved.contact_id, now_unix_ms)?;
                    }
                    if should_send_approval_signal
                        && session.bootstrap_state
                            == omni_crypto::SessionBootstrapState::Established
                    {
                        let _session_guard = lock_app_session_guard(&self.session_lock)?;
                        if let Some(session) =
                            reload_established_session_under_lock(&self.storage, session)?
                        {
                            let _ = send_contact_approval_signal_for_session(
                                &self.storage,
                                &self.transport,
                                &session,
                                target_dialog_key,
                            );
                        }
                    }
                } else {
                    // (Both the Pending and non-Pending cases refreshed identically, so the
                    // distinction was inert — collapsed. If they should differ, that's a separate fix.)
                    let _ = self.refresh_contact_request_on_active_transport();
                }
            }
            if previous_contact.relationship_state != ContactRelationshipState::Approved {
                if should_send_approval_signal {
                    if let Some(dialog_key) = target_dialog_key {
                        schedule_contact_approval_signal_retry_for_active_transport(
                            &self.storage,
                            Arc::clone(&self.transport_handle),
                            Arc::clone(&self.session_lock),
                            &approved.contact_id,
                            dialog_key,
                        );
                    }
                }
                if request_signal_confirmed {
                    if let (Some(dialog_key), Some(peer_id)) =
                        (target_dialog_key, snapshot.peer_id.as_deref())
                    {
                        schedule_verification_challenge_retry_for_active_transport(
                            &self.storage,
                            Arc::clone(&self.transport_handle),
                            Arc::clone(&self.session_lock),
                            &approved.contact_id,
                            dialog_key,
                            peer_id,
                        );
                    } else {
                        let _session_guard = lock_app_session_guard(&self.session_lock)?;
                        let _ = maybe_send_verification_challenge_for_active_transport(
                            &self.storage,
                            &self.transport,
                        );
                    }
                }
            } else {
                let _session_guard = lock_app_session_guard(&self.session_lock)?;
                let _ = maybe_send_verification_challenge_for_active_transport(
                    &self.storage,
                    &self.transport,
                );
            }
        }
        // Mesh fallback (relay path). A contact reachable only over the mesh — e.g. via a relay —
        // never matches the legacy "active transport", so the approval-signal block above is skipped
        // entirely: the approver flips the contact to Approved but never tells the peer, leaving the
        // peer stuck Pending forever (later approve-all calls are no-ops). When we just approved an
        // INCOMING request that wasn't bound to the active legacy transport, deliver the approval
        // signal over the mesh carrier (which routes direct or via relay). Anti-spam intent preserved:
        // only for a contact that sent us an incoming request and that we just approved. The carrier
        // no-ops when there's no mesh link, so this is safe on any path.
        // See docs/bugs/relay-pairing-approval-gap.md.
        if !matched_active_transport
            && previous_contact.relationship_state == ContactRelationshipState::Pending
            && previous_contact.pending_request_direction == Some(ContactRequestDirection::Incoming)
        {
            if let (Some(remote_account_id), Some(dialog_key)) = (
                approved.remote_account_id.as_ref(),
                approved.primary_dialog_key.as_deref(),
            ) {
                if let Ok(Some(session)) = self
                    .storage
                    .get_session_for_dialog(remote_account_id, dialog_key)
                {
                    if session.bootstrap_state == omni_crypto::SessionBootstrapState::Established {
                        let db_path = self.storage.db_path().to_string_lossy().to_string();
                        let _ =
                            mark_runtime_chat_session_approval_signal_pending(&db_path, dialog_key);
                        let _session_guard = lock_app_session_guard(&self.session_lock)?;
                        omni_app_trace_log(
                            "approve/mesh_fallback",
                            &format!("send approval over mesh dialog_key={dialog_key}"),
                        );
                        let sent = match reload_established_session_under_lock(
                            &self.storage,
                            &session,
                        )? {
                            Some(session) => send_contact_approval_signal_for_session(
                                &self.storage,
                                &self.transport,
                                &session,
                                Some(dialog_key),
                            )
                            .is_ok(),
                            None => false,
                        };
                        if sent {
                            let _ = clear_runtime_chat_session_approval_signal_pending(
                                &db_path, dialog_key,
                            );
                        }
                    }
                }
            }
        }
        if approved.primary_dialog_key.is_some() {
            let _ = self
                .storage
                .record_contact_presence_activity(&approved.contact_id, now_unix_ms);
        }
        self.push_contact_change_to_own_devices();

        Ok(approved)
    }

    pub fn decline_contact(&self, contact_id: &ContactId) -> OmniResult<ContactSnapshot> {
        let _ = self.ensure_runtime_profile_active("decline_contact")?;
        let declined = self.storage.decline_contact(contact_id, current_unix_ms())?;
        self.push_contact_change_to_own_devices();
        Ok(declined)
    }

    pub fn forget_contact(&self, contact_id: &ContactId) -> OmniResult<ContactSnapshot> {
        let _ = self.ensure_runtime_profile_active("forget_contact")?;
        let _session_guard = lock_app_session_guard(&self.session_lock)?;
        let removed = self.storage.forget_contact(contact_id, current_unix_ms())?;
        if let Some(remote_account_id) = removed.remote_account_id.as_ref() {
            self.storage
                .delete_session_state_for_remote_account(remote_account_id)?;
        }
        drop_mesh_link_for_removed_contact(self, &removed);
        let _ = clear_verification_challenges_for_contact(&self.storage, &removed);
        let _ = clear_runtime_chat_session_for_contact(&self.storage, &removed);
        let _ = clear_runtime_contact_ephemera_for_active_profile(&self.storage);
        drop(_session_guard);
        self.push_contact_change_to_own_devices();
        Ok(removed)
    }

    pub fn forget_contact_and_reset_peer(
        &self,
        contact_id: &ContactId,
    ) -> OmniResult<ContactSnapshot> {
        let _ = self.ensure_runtime_profile_active("forget_contact_and_reset_peer")?;
        let _session_guard = lock_app_session_guard(&self.session_lock)?;
        let contact = self.storage.get_contact(contact_id)?;
        let dialog_keys = contact_dialog_keys_for_peer_reset(&contact);
        let active_snapshot = self.transport.snapshot();

        for dialog_key in &dialog_keys {
            let _ = pause_chat_session_for_profile(&self.storage, dialog_key);
            let _ = send_peer_reset_pause_notice(&self.storage, &self.transport, dialog_key);
        }
        reset_transport_for_contact(self, &contact, &dialog_keys, &active_snapshot)?;

        let removed = self.storage.forget_contact(contact_id, current_unix_ms())?;
        if let Some(remote_account_id) = removed.remote_account_id.as_ref() {
            self.storage
                .delete_session_state_for_remote_account(remote_account_id)?;
        }
        drop_mesh_link_for_removed_contact(self, &removed);
        clear_verification_challenges_for_contact(&self.storage, &removed)?;
        clear_runtime_chat_session_for_contact(&self.storage, &removed)?;
        clear_runtime_contact_ephemera_for_active_profile(&self.storage)?;
        drop(_session_guard);
        self.push_contact_change_to_own_devices();
        Ok(removed)
    }

    /// A contact changed by hand: our other devices hear it now rather than on the next reconcile
    /// pass — this device may be shut right after (a removal on a laptop that then sleeps). Best
    /// effort: what does not get through now goes on the reconcile pass or the next link.
    fn push_contact_change_to_own_devices(&self) {
        if let Ok(_session_guard) = lock_app_session_guard(&self.session_lock) {
            sync_contacts_with_own_devices(&self.storage, &self.transport, current_unix_ms());
        }
        // And a note at the couriers for a device of ours that is offline now.
        account_mailbox_round(&self.storage, false);
    }

    pub fn list_contacts(&self) -> OmniResult<Vec<ContactSnapshot>> {
        self.storage.list_contacts()
    }

    pub fn get_contacts_refresh_summary(&self) -> OmniResult<omni_types::ContactsRefreshSummary> {
        self.storage.get_contacts_refresh_summary()
    }

    pub fn get_contacts_screen_snapshot(&self) -> OmniResult<ContactsScreenSnapshot> {
        Ok(ContactsScreenSnapshot {
            refresh_summary: self.storage.get_contacts_refresh_summary()?,
            contacts: self.storage.list_contacts()?,
        })
    }

    pub fn get_contact_card(&self, contact_id: &ContactId) -> OmniResult<ContactCardSnapshot> {
        self.storage.get_contact_card(contact_id)
    }

    pub fn get_contact_card_revision(
        &self,
        contact_id: &ContactId,
    ) -> OmniResult<Option<omni_types::EntityRevisionSnapshot>> {
        self.storage.get_contact_card_revision(contact_id)
    }

    pub fn verify_contact(
        &self,
        contact_id: &ContactId,
        method: VerificationMethod,
    ) -> OmniResult<ContactCardSnapshot> {
        let _ = self.ensure_runtime_profile_active("verify_contact")?;
        self.storage
            .verify_contact(contact_id, method, current_unix_ms())
    }

    pub fn record_contact_observation(
        &self,
        contact_id: &ContactId,
        observation: &ContactObservationSnapshot,
    ) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("record_contact_observation")?;
        self.storage
            .record_contact_observation(contact_id, observation)
    }

    pub fn export_my_verification_payload(&self) -> OmniResult<MyVerificationPayloadV1> {
        let _ = self.ensure_runtime_profile_active("export_my_verification_payload")?;
        // One builder for the shell's card and the over-the-channel answer: the two used to be copies.
        build_local_verification_payload(&self.storage)
    }

    pub fn get_my_verification_payload(&self) -> OmniResult<MyVerificationPayloadV1> {
        self.export_my_verification_payload()
    }

    pub fn inspect_remote_verification_payload(
        &self,
        payload: &ContactVerificationPayloadV1,
    ) -> OmniResult<ImportableContactPayloadV1> {
        validate_contact_verification_payload_v1(payload)?;
        importable_contact_payload_from_verification(payload)
    }

    pub fn add_contact_from_payload(
        &self,
        payload: &ImportableContactPayloadV1,
        petname: impl Into<String>,
    ) -> OmniResult<ContactSnapshot> {
        let _ = self.ensure_runtime_profile_active("add_contact_from_payload")?;
        omni_protocol::validate_importable_contact_payload_v1(payload)?;

        let observation = observation_from_contact_payload(payload);
        self.add_contact(&AddContactInput {
            petname: petname.into(),
            remote_account_id: Some(omni_identity::AccountId(payload.account.account_id.clone())),
            public_display_name: Some(payload.account.display_name.clone()),
            remote_instance_id: None,
            primary_dialog_key: None,
            initial_observation: Some(observation),
        })
    }

    pub fn confirm_contact_verification(
        &self,
        contact_id: &ContactId,
        payload: &ContactVerificationPayloadV1,
        method: VerificationMethod,
    ) -> OmniResult<ContactCardSnapshot> {
        let _ = self.ensure_runtime_profile_active("confirm_contact_verification")?;
        validate_contact_verification_payload_v1(payload)?;
        let observation = observation_from_verification_payload(payload);
        self.storage
            .record_contact_observation(contact_id, &observation)?;
        self.storage
            .verify_contact(contact_id, method, current_unix_ms())
    }

    pub fn get_contact_verification_state(
        &self,
        contact_id: &ContactId,
    ) -> OmniResult<ContactCardSnapshot> {
        self.get_contact_card(contact_id)
    }
}

fn contact_dialog_keys_for_peer_reset(contact: &ContactSnapshot) -> Vec<String> {
    let mut dialog_keys = Vec::new();
    if let Some(primary_dialog_key) = contact.primary_dialog_key.as_deref() {
        push_unique_dialog_key(&mut dialog_keys, primary_dialog_key);
    }
    if let Some(remote_instance_id) = contact.remote_instance_id.as_deref() {
        push_unique_dialog_key(&mut dialog_keys, &dialog_key_from_instance_id(remote_instance_id));
    }
    if let Some(remote_account_id) = contact.remote_account_id.as_ref() {
        push_unique_dialog_key(
            &mut dialog_keys,
            &omni_types::dialog_key_from_account_id(&remote_account_id.0),
        );
    }
    dialog_keys
}

fn push_unique_dialog_key(dialog_keys: &mut Vec<String>, dialog_key: &str) {
    let dialog_key = normalize_dialog_key(dialog_key);
    if dialog_key.trim().is_empty() || dialog_keys.contains(&dialog_key) {
        return;
    }
    dialog_keys.push(dialog_key);
}

fn send_peer_reset_pause_notice(
    storage: &Storage,
    transport: &TransportManager,
    dialog_key: &str,
) -> OmniResult<bool> {
    let Some(connection) = transport.connection_snapshot_for_dialog(dialog_key) else {
        return Ok(false);
    };
    let (Some(remote_account_id), Some(remote_device_id)) = (
        connection.remote_account_id.as_deref(),
        connection.remote_device_id.as_deref(),
    ) else {
        return Ok(false);
    };

    let payload = omni_types::WirePayloadV1::LogicalEvent(
        omni_types::LogicalEventEnvelopeV1::pause_auto_reconnect(current_unix_ms()),
    );
    let Ok(mut outgoing) = prepare_encrypted_payload_for_session_address(
        storage,
        &omni_crypto::SessionAddress {
            remote_account_id: AccountId(remote_account_id.to_string()),
            remote_device_id: DeviceId(remote_device_id.to_string()),
        },
        &payload,
        SecurityPayloadKind::PauseAutoReconnect,
    ) else {
        return Ok(false);
    };

    storage.store_session_record(&mut outgoing.updated_session)?;
    Ok(transport
        .send_wire_envelope_for_dialog_key(dialog_key, &outgoing.envelope)
        .is_ok())
}

fn reset_transport_for_contact(
    app: &OmniApp,
    contact: &ContactSnapshot,
    dialog_keys: &[String],
    active_snapshot: &TransportSnapshot,
) -> OmniResult<()> {
    if !transport_snapshot_matches_contact(active_snapshot, contact, dialog_keys)
        && !app.active_transport_matches_contact(contact)
    {
        return Ok(());
    }

    for dialog_key in dialog_keys {
        if app.transport.connection_snapshot_for_dialog(dialog_key).is_some() {
            app.transport.disconnect_dialog_and_reset(dialog_key)?;
        }
    }

    let current_snapshot = app.transport.snapshot();
    if transport_snapshot_matches_contact(&current_snapshot, contact, dialog_keys)
        || app.active_transport_matches_contact(contact)
    {
        app.transport.disconnect_and_reset()?;
    }
    Ok(())
}

fn transport_snapshot_matches_contact(
    snapshot: &TransportSnapshot,
    contact: &ContactSnapshot,
    dialog_keys: &[String],
) -> bool {
    if !matches!(
        snapshot.state,
        TransportState::Connected | TransportState::ListeningAndConnected
    ) {
        return false;
    }
    if snapshot
        .dialog_key
        .as_ref()
        .is_some_and(|dialog_key| dialog_keys.iter().any(|known| known == dialog_key))
    {
        return true;
    }
    if contact
        .remote_instance_id
        .as_ref()
        .is_some_and(|remote_instance_id| {
            snapshot.remote_instance_id.as_deref() == Some(remote_instance_id.as_str())
        })
    {
        return true;
    }
    contact
        .remote_account_id
        .as_ref()
        .is_some_and(|remote_account_id| {
            snapshot.remote_account_id.as_deref() == Some(remote_account_id.0.as_str())
        })
}

/// Drop the mesh Link + runtime registries for a contact that was just removed via
/// [`OmniApp::forget_contact`] / [`OmniApp::forget_contact_and_reset_peer`].
///
/// Bug A/B (2026-07-20): a peer that shares a live mesh link at the moment of forget keeps that
/// QUIC connection warm. `Network::dial` then coalesces onto it and emits no fresh
/// `SessionHello`, so a later re-add (or the peer's own re-request) can't complete the handshake
/// until an app restart tore the link down. We drop OUR view of the link + the stale routing
/// registries here, so the next dial re-runs the full `register_link` → hello path.
///
/// Deliberately does NOT block the peer from re-requesting: if the far side still holds us as a
/// contact and sends a fresh request, it correctly reappears as a NEW Pending the user can
/// approve or decline (asserted by `forgotten_contact_can_request_again_over_existing_connection`).
/// A local veto there would break that designed re-request flow — a forgotten contact coming back
/// as Pending is honest, not a resurrection.
///
/// Best-effort by design: none of the state we clear is authoritative (storage already deleted
/// the contact row), so a partial failure just leaves stale runtime bookkeeping that ages out.
fn drop_mesh_link_for_removed_contact(app: &OmniApp, removed: &ContactSnapshot) {
    let Some(remote_account_id) = removed.remote_account_id.as_ref() else {
        return;
    };
    // Resolve the peer's mesh node from its prekey bundle, then tear down runtime state.
    let Ok(Some(bundle)) = app
        .storage
        .get_latest_remote_prekey_bundle_for_account(remote_account_id)
    else {
        return;
    };
    let Ok(peer_node) = node_address_from_ed25519_pubkey(bundle.identity_signing_key.0) else {
        return;
    };

    if let Some(runtime) = app.mesh_runtime() {
        let dropped = runtime.drop_link(&peer_node);
        omni_app_trace_log(
            "forget/mesh_drop",
            &format!(
                "peer={peer_node} account={} link_dropped={dropped}",
                remote_account_id.0
            ),
        );
    }
    if let Ok(mut targets) = app.mesh_pairing_targets.lock() {
        targets.remove(&peer_node);
    }
    if let Ok(mut dialed) = app.mesh_locally_dialed.lock() {
        dialed.remove(&peer_node);
    }
    if let Ok(mut ctxs) = app.mesh_node_ctx.lock() {
        ctxs.remove(&peer_node);
    }
    if let Ok(mut map) = app.mesh_dialogs.lock() {
        map.forget_node(&peer_node);
    }
}
