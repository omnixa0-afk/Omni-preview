/// Assemble the shareable wire `PrekeyBundleV1` (all keys as hex/base64 strings) from the
/// raw `omni_crypto::PrekeyBundle` (byte fields) plus the local device label. This is the
/// ONE canonical producer of the wire bundle form: both `get_local_prekey_bundle` (GUI /
/// manual exchange) and the Model A identity-beacon publish (`publish_identity_beacon`)
/// serialize THIS, so the find-by-ID side — which deserializes `PrekeyBundleV1` — round-trips.
/// Serializing the raw `PrekeyBundle` instead emits JSON byte arrays that fail V1 parsing
/// ("invalid type: sequence, expected a string"); that bug shipped because MA2 serialized the
/// raw bundle and only an in-process test double (not the wire types) exercised the path.
pub(crate) fn prekey_bundle_v1_from_raw(
    bundle: &omni_crypto::PrekeyBundle,
    device_label: String,
) -> PrekeyBundleV1 {
    let signed_prekey_signature = signature_to_wire(bundle.signed_prekey.signature.clone());
    PrekeyBundleV1 {
        version: PREKEY_BUNDLE_VERSION_V1,
        account: AccountCryptoDescriptorV1 {
            version: ACCOUNT_CRYPTO_DESCRIPTOR_VERSION_V1,
            account_id: bundle.account_id.0.clone(),
            identity_public_key: identity_public_key_to_wire(bundle.identity_key),
            identity_signing_public_key: identity_signing_public_key_to_wire(
                bundle.identity_signing_key,
            ),
        },
        device: DeviceCryptoDescriptorV1 {
            version: DEVICE_CRYPTO_DESCRIPTOR_VERSION_V1,
            device_id: bundle.device_id.0.clone(),
            account_id: bundle.account_id.0.clone(),
            device_label,
            device_public_key: device_public_key_to_wire(bundle.device_key),
        },
        signed_prekey: omni_protocol::SignedPrekeyDescriptorV1 {
            prekey_id: bundle.signed_prekey.prekey_id,
            public_key: prekey_public_key_to_wire(bundle.signed_prekey.public_key),
            signature_r: signed_prekey_signature.0,
            signature_s: signed_prekey_signature.1,
            expires_at_unix_ms: bundle.signed_prekey.expires_at_unix_ms,
        },
        one_time_prekey: bundle
            .one_time_prekey
            .clone()
            .map(|value| OneTimePrekeyDescriptorV1 {
                prekey_id: value.prekey_id,
                public_key: prekey_public_key_to_wire(value.public_key),
            }),
        generated_at_unix_ms: bundle.generated_at_unix_ms.max(current_unix_ms()),
        device_certificate_chain_json: bundle
            .device_certificate_chain
            .as_ref()
            .and_then(|chain| serde_json::to_string(chain).ok()),
    }
}

/// This device's certificate chain as carried on the wire (in a session bootstrap).
fn local_device_certificate_chain_json(storage: &Storage) -> OmniResult<Option<String>> {
    storage
        .get_local_device_certificate_chain()?
        .map(|chain| serde_json::to_string(&chain).map_err(|error| OmniError::Internal(error.to_string())))
        .transpose()
}

impl OmniApp {
    fn wait_briefly_for_active_transport_request_context(
        &self,
    ) -> (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<PrekeyBundleV1>,
    ) {
        #[cfg(not(test))]
        let attempts = 8;
        #[cfg(test)]
        let attempts = 12;
        #[cfg(not(test))]
        let delay_ms = 25;
        #[cfg(test)]
        let delay_ms = 10;

        let mut remote_account_id = self.transport.active_remote_account_id();
        let mut active_dialog_key = self.transport.active_dialog_key();
        let mut active_remote_instance_id = self.transport.active_remote_instance_id();
        let mut active_remote_bundle = self.transport.active_remote_prekey_bundle();

        for _ in 0..attempts {
            let has_established_session = remote_account_id
                .as_ref()
                .and_then(|value| {
                    self.storage
                        .get_latest_session_for_remote_account(&AccountId(value.clone()))
                        .ok()
                        .flatten()
                })
                .is_some_and(|session| {
                    session.bootstrap_state == omni_crypto::SessionBootstrapState::Established
                });
            if remote_account_id.is_some() && (active_remote_bundle.is_some() || has_established_session)
            {
                break;
            }

            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            remote_account_id = self.transport.active_remote_account_id();
            active_dialog_key = self.transport.active_dialog_key();
            active_remote_instance_id = self.transport.active_remote_instance_id();
            active_remote_bundle = self.transport.active_remote_prekey_bundle();
        }

        (
            remote_account_id,
            active_dialog_key,
            active_remote_instance_id,
            active_remote_bundle,
        )
    }

    fn wait_briefly_for_established_session_for_remote_account(
        &self,
        remote_account: &AccountId,
    ) -> OmniResult<bool> {
        #[cfg(not(test))]
        let attempts = 12;
        #[cfg(test)]
        let attempts = 80;
        #[cfg(not(test))]
        let delay_ms = 25;
        #[cfg(test)]
        let delay_ms = 25;

        for _ in 0..attempts {
            if self
                .storage
                .get_latest_session_for_remote_account(remote_account)?
                .is_some_and(|session| {
                    session.bootstrap_state == omni_crypto::SessionBootstrapState::Established
                })
            {
                return Ok(true);
            }

            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        }

        Ok(false)
    }

    fn send_contact_request_signal_for_session(
        &self,
        session: &omni_crypto::SessionRecord,
        target_dialog_key: Option<&str>,
    ) -> OmniResult<omni_crypto::SessionRecord> {
        let now_unix_ms = current_unix_ms();
        omni_app_trace_log(
            "request_signal/send",
            &format!(
                "site=app_crypto bootstrap_state={:?} target_dialog_key={:?}",
                session.bootstrap_state, target_dialog_key
            ),
        );
        let mut outgoing = omni_messaging::build_encrypted_logical_event_envelope(
            session,
            &omni_types::LogicalEventEnvelopeV1::contact_presence_ping(
                format!("{CONTACT_REQUEST_SIGNAL_PREFIX}{now_unix_ms}"),
                now_unix_ms,
            ),
            SecurityPayloadKind::ContactPresencePing,
            now_unix_ms,
        )?;
        self.storage.store_session_record(&mut outgoing.updated_session)?;
        if let Some(dialog_key) = target_dialog_key {
            self.transport
                .send_wire_envelope_for_dialog_key(dialog_key, &outgoing.envelope)?;
            let _ = mark_runtime_chat_session_request_signal_sent(
                &self.storage.db_path().to_string_lossy(),
                dialog_key,
            );
            omni_app_trace_log(
                "request_signal/sent",
                &format!("site=app_crypto via=dialog_key dialog_key={dialog_key}"),
            );
        } else {
            self.transport.send_wire_envelope(&outgoing.envelope)?;
            omni_app_trace_log(
                "request_signal/sent",
                "site=app_crypto via=wire_envelope dialog_key=none",
            );
        }
        Ok(outgoing.updated_session)
    }

    pub fn get_local_prekey_bundle(&self) -> OmniResult<PrekeyBundleV1> {
        let _ = self.ensure_runtime_profile_active("get_local_prekey_bundle")?;
        let active_profile = self.current_active_profile_context_required()?;
        let device_label = active_profile.device.device_label;
        self.storage.ensure_local_crypto_foundation(current_unix_ms())?;
        let bundle = self.storage.build_local_prekey_bundle(current_unix_ms())?;
        Ok(prekey_bundle_v1_from_raw(&bundle, device_label))
    }

    pub fn set_local_prekey_bundle_json(&self, bundle_json: Option<String>) {
        self.transport.set_local_prekey_bundle_json(bundle_json);
    }

    pub fn store_remote_prekey_bundle(&self, bundle: &PrekeyBundleV1) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("store_remote_prekey_bundle")?;
        validate_prekey_bundle_v1(bundle)?;
        let domain_bundle = prekey_bundle_from_protocol(bundle)?;
        self.storage.record_remote_prekey_bundle(
            &domain_bundle,
            None,
            Some(&bundle.device.device_label),
            bundle.generated_at_unix_ms,
        )
    }

    pub fn bootstrap_session_from_prekey_bundle(
        &self,
        bundle: &PrekeyBundleV1,
    ) -> OmniResult<SessionBootstrapMessageV1> {
        let _ = self.ensure_runtime_profile_active("bootstrap_session_from_prekey_bundle")?;
        validate_prekey_bundle_v1(bundle)?;
        self.storage.ensure_local_crypto_foundation(current_unix_ms())?;

        let domain_bundle = prekey_bundle_from_protocol(bundle)?;
        self.storage.record_remote_prekey_bundle(
            &domain_bundle,
            None,
            Some(&bundle.device.device_label),
            bundle.generated_at_unix_ms,
        )?;

        let active_profile = self.current_active_profile_context_required()?;
        let account = active_profile.account;
        let device = active_profile.device;
        let local_identity_key = self
            .storage
            .get_local_identity_key(&account.descriptor.account_id)?
            .ok_or_else(|| OmniError::Storage("local identity key is missing".to_string()))?;
        let local_device_key = self
            .storage
            .get_local_device_key(&device.device_id)?
            .ok_or_else(|| OmniError::Storage("local device key is missing".to_string()))?;

        let (mut session, bootstrap) = bootstrap_initiator_session(
            &local_identity_key,
            &local_device_key,
            &domain_bundle,
            current_unix_ms(),
        )?;
        self.storage.replace_session_record(&mut session)?;
        let replay_window = self
            .storage
            .ensure_replay_window(&session.session_address, current_unix_ms())?;
        self.storage.store_replay_window(&replay_window)?;

        Ok(SessionBootstrapMessageV1 {
            version: SESSION_BOOTSTRAP_MESSAGE_VERSION_V1,
            initiator_account_id: bootstrap.initiator_account_id.0,
            initiator_device_id: bootstrap.initiator_device_id.0,
            initiator_identity_public_key: identity_public_key_to_wire(
                bootstrap.initiator_identity_key,
            ),
            initiator_identity_signing_public_key: identity_signing_public_key_to_wire(
                bootstrap.initiator_identity_signing_key,
            ),
            initiator_device_public_key: device_public_key_to_wire(
                bootstrap.initiator_device_key,
            ),
            initiator_ephemeral_public_key: ephemeral_public_key_to_wire(
                bootstrap.initiator_ephemeral_key,
            ),
            remote_signed_prekey_id: bootstrap.remote_signed_prekey_id,
            remote_one_time_prekey_id: bootstrap.remote_one_time_prekey_id,
            bootstrap_signature_r: signature_to_wire(bootstrap.bootstrap_signature.clone()).0,
            bootstrap_signature_s: signature_to_wire(bootstrap.bootstrap_signature.clone()).1,
            created_at_unix_ms: bootstrap.created_at_unix_ms,
            // Same instance id as our SessionHello, so a receiver processing a hello-less
            // (durable, first-contact) bootstrap derives the identical dialog_key.
            initiator_instance_id: self.storage.get_or_create_local_instance_id()?,
            initiator_device_certificate_chain_json: local_device_certificate_chain_json(
                &self.storage,
            )?,
        })
    }

    pub fn accept_session_bootstrap(
        &self,
        message: &SessionBootstrapMessageV1,
    ) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("accept_session_bootstrap")?;
        validate_session_bootstrap_message_v1(message)?;
        self.storage.ensure_local_crypto_foundation(current_unix_ms())?;

        let active_profile = self.current_active_profile_context_required()?;
        let account = active_profile.account;
        let device = active_profile.device;
        let local_identity_key = self
            .storage
            .get_local_identity_key(&account.descriptor.account_id)?
            .ok_or_else(|| OmniError::Storage("local identity key is missing".to_string()))?;
        let local_device_key = self
            .storage
            .get_local_device_key(&device.device_id)?
            .ok_or_else(|| OmniError::Storage("local device key is missing".to_string()))?;
        let session_address = omni_crypto::SessionAddress {
            remote_account_id: AccountId(message.initiator_account_id.clone()),
            remote_device_id: DeviceId(message.initiator_device_id.clone()),
        };
        let has_approved_contact = self
            .storage
            .find_contact_by_remote_account_id(&AccountId(message.initiator_account_id.clone()))?
            .is_some_and(|contact| {
                contact.relationship_state == omni_identity::ContactRelationshipState::Approved
            });
        // Same collision-echo guard as the transport path: read the peer's newest-seen bootstrap
        // mark before recording this one, and record it either way so re-deliveries are recognised.
        let db_path = self.storage.db_path().to_string_lossy().to_string();
        let last_seen_bootstrap_created_at = last_inbound_bootstrap_created_at(
            &db_path,
            &message.initiator_account_id,
            &message.initiator_device_id,
        );
        record_inbound_bootstrap_created_at(
            &db_path,
            &message.initiator_account_id,
            &message.initiator_device_id,
            message.created_at_unix_ms,
        );
        if let Some(existing_session) = self.storage.get_session_record(&session_address)? {
            if existing_session_should_absorb_incoming_bootstrap(
                &account.descriptor.account_id,
                &device.device_id,
                &existing_session,
                message,
                has_approved_contact,
                last_seen_bootstrap_created_at,
            ) {
                return Ok(());
            }
        }
        let signed_prekey = self
            .storage
            .get_active_signed_prekey(&device.device_id)?
            .ok_or_else(|| OmniError::Storage("active signed prekey is missing".to_string()))?;
        let one_time_prekey = match message.remote_one_time_prekey_id {
            Some(expected_id) => {
                let candidate = self
                    .storage
                    .get_one_time_prekey_for_inbound_bootstrap(&device.device_id, expected_id, current_unix_ms())?
                    .ok_or_else(|| {
                        OmniError::Storage("expected one-time prekey is missing".to_string())
                    })?;
                Some(candidate)
            }
            None => None,
        };

        let bootstrap = session_bootstrap_from_protocol(message)?;
        let session = bootstrap_responder_session(
            &account.descriptor.account_id,
            &local_identity_key,
            &local_device_key,
            &signed_prekey,
            one_time_prekey.as_ref(),
            &bootstrap,
            current_unix_ms(),
        )?;
        let mut session = establish_session(&session, current_unix_ms());
        self.storage.replace_session_record(&mut session)?;
        let replay_window = self
            .storage
            .ensure_replay_window(&session.session_address, current_unix_ms())?;
        self.storage.store_replay_window(&replay_window)?;

        if let Some(prekey) = one_time_prekey {
            self.storage
                .mark_one_time_prekey_consumed(
                    &prekey.device_id,
                    prekey.prekey_id,
                    current_unix_ms(),
                )?;
        }

        Ok(())
    }

    pub fn refresh_contact_request_on_active_transport(&self) -> OmniResult<TransportCommandResult> {
        let _ = self.ensure_runtime_profile_active("refresh_contact_request_on_active_transport")?;
        let now_unix_ms = current_unix_ms();
        omni_app_trace_log("refresh_request/entry", "");
        let (
            remote_account_id,
            active_dialog_key,
            active_remote_instance_id,
            active_remote_bundle,
        ) = self.wait_briefly_for_active_transport_request_context();
        let remote_account_id = remote_account_id
            .ok_or_else(|| OmniError::Transport("no active connected contact".to_string()))?;
        if let Some(dialog_key) = active_dialog_key.as_deref() {
            let _ = resume_chat_session_lease_for_profile(&self.storage, dialog_key);
            let _ = touch_runtime_chat_session_interaction(
                &self.storage.db_path().to_string_lossy(),
                dialog_key,
                omni_types::ChatSessionInteractionKind::Session,
                now_unix_ms,
            );
        }
        let remote_account = AccountId(remote_account_id.clone());
        let existing_petname = self
            .storage
            .find_contact_by_remote_account_id(&remote_account)?
            .map(|contact| contact.petname)
            .unwrap_or_else(|| remote_account_id.clone());

        let exact_active_session = active_remote_bundle
            .as_ref()
            .map(|bundle| {
                self.storage.get_session_record(&omni_crypto::SessionAddress {
                    remote_account_id: AccountId(bundle.account.account_id.clone()),
                    remote_device_id: DeviceId(bundle.device.device_id.clone()),
                })
            })
            .transpose()?
            .flatten()
            .filter(|value| {
                value.bootstrap_state == omni_crypto::SessionBootstrapState::Established
            });
        // The request goes out on the active dialog, so the session must be that dialog's device's.
        let active_dialog_session = match active_dialog_key.as_deref() {
            Some(dialog_key) => self.storage.get_session_for_dialog(&remote_account, dialog_key)?,
            None => self.storage.get_latest_session_for_remote_account(&remote_account)?,
        };
        let fallback_established_session = active_dialog_session.filter(|value| {
            value.bootstrap_state == omni_crypto::SessionBootstrapState::Established
        });

        let remote_bundle = if let Some(bundle) = active_remote_bundle {
            if bundle.account.account_id != remote_account.0 {
                return Err(OmniError::Transport(
                    "active transport bundle does not match the connected remote account"
                        .to_string(),
                ));
            }
            let domain_bundle = prekey_bundle_from_protocol(&bundle)?;
            self.storage.record_remote_prekey_bundle(
                &domain_bundle,
                None,
                Some(&bundle.device.device_label),
                bundle.generated_at_unix_ms,
            )?;
            Some(domain_bundle)
        } else { self
            .storage
            .get_latest_remote_prekey_bundle_for_account(&remote_account)? };
        let Some(mut remote_bundle) = remote_bundle else {
            if let Some(session) = exact_active_session.or(fallback_established_session) {
                omni_app_trace_log(
                    "refresh_request/branch",
                    &format!(
                        "no_remote_bundle+established_session bootstrap_state={:?} dialog_key={:?}",
                        session.bootstrap_state, active_dialog_key
                    ),
                );
                let _session_guard = lock_app_session_guard(&self.session_lock)?;
                let contact = self.storage.add_outgoing_pending_contact(
                    &AddContactInput {
                        petname: existing_petname,
                        remote_account_id: Some(remote_account.clone()),
                        public_display_name: None,
                        remote_instance_id: active_remote_instance_id.clone(),
                        primary_dialog_key: active_dialog_key.clone(),
                        initial_observation: None,
                    },
                    now_unix_ms,
                )?;
                self.storage
                    .record_contact_presence_activity(&contact.contact_id, now_unix_ms)?;
                let session = reload_established_session_under_lock(&self.storage, &session)?
                    .unwrap_or(session);
                let _ = self.send_contact_request_signal_for_session(
                    &session,
                    active_dialog_key.as_deref(),
                )?;
                return Ok(TransportCommandResult {
                    ok: true,
                    message: format!(
                        "contact request refreshed for {remote_account_id} over the active secure session"
                    ),
                    snapshot: self.transport.snapshot(),
                });
            }

            omni_app_trace_log(
                "refresh_request/branch",
                "no_remote_bundle+NO_established_session — Err returned",
            );
            return Err(OmniError::Transport(
                "no remote prekey bundle is available for the active contact".to_string(),
            ));
        };
        remote_bundle.one_time_prekey = None;

        {
            let _session_guard = lock_app_session_guard(&self.session_lock)?;
            let contact = self.storage.add_outgoing_pending_contact(
                &AddContactInput {
                    petname: existing_petname,
                    remote_account_id: Some(remote_account.clone()),
                    public_display_name: None,
                    remote_instance_id: active_remote_instance_id.clone(),
                    primary_dialog_key: active_dialog_key.clone(),
                    initial_observation: None,
                },
                now_unix_ms,
            )
            .map_err(|error| {
                OmniError::Storage(format!(
                    "failed to create pending contact for explicit-route refresh: {error}"
                ))
            })?;
            self.storage
                .record_contact_presence_activity(&contact.contact_id, now_unix_ms)
                .map_err(|error| {
                    OmniError::Storage(format!(
                        "failed to record pending contact activity during explicit-route refresh: {error}"
                    ))
                })?;
            self.storage.ensure_local_crypto_foundation(now_unix_ms)?;
            self.storage
                .delete_session_state_for_remote_account(&remote_account)
                .map_err(|error| {
                    OmniError::Storage(format!(
                        "failed to clear stale session state during explicit-route refresh: {error}"
                    ))
                })?;
            let _ = clear_runtime_contact_ephemera_for_active_profile(&self.storage);
            let active_profile = self.current_active_profile_context_required()?;
            let local_identity_key = self
                .storage
                .get_local_identity_key(&active_profile.account.descriptor.account_id)?
                .ok_or_else(|| OmniError::Storage("local identity key is missing".to_string()))?;
            let local_device_key = self
                .storage
                .get_local_device_key(&active_profile.device.device_id)?
                .ok_or_else(|| OmniError::Storage("local device key is missing".to_string()))?;

            let (mut session, bootstrap) = bootstrap_initiator_session(
                &local_identity_key,
                &local_device_key,
                &remote_bundle,
                now_unix_ms,
            )?;
            self.storage.replace_session_record(&mut session).map_err(|error| {
                OmniError::Storage(format!(
                    "failed to store initiator session during explicit-route refresh: {error}"
                ))
            })?;
            let replay_window = self
                .storage
                .ensure_replay_window(&session.session_address, now_unix_ms)
                .map_err(|error| {
                    OmniError::Storage(format!(
                        "failed to create replay window during explicit-route refresh: {error}"
                    ))
                })?;
            self.storage.store_replay_window(&replay_window).map_err(|error| {
                OmniError::Storage(format!(
                    "failed to store replay window during explicit-route refresh: {error}"
                ))
            })?;

            let bootstrap_v1 = SessionBootstrapMessageV1 {
                version: SESSION_BOOTSTRAP_MESSAGE_VERSION_V1,
                initiator_account_id: bootstrap.initiator_account_id.0,
                initiator_device_id: bootstrap.initiator_device_id.0,
                initiator_identity_public_key: identity_public_key_to_wire(
                    bootstrap.initiator_identity_key,
                ),
                initiator_identity_signing_public_key: identity_signing_public_key_to_wire(
                    bootstrap.initiator_identity_signing_key,
                ),
                initiator_device_public_key: device_public_key_to_wire(
                    bootstrap.initiator_device_key,
                ),
                initiator_ephemeral_public_key: ephemeral_public_key_to_wire(
                    bootstrap.initiator_ephemeral_key,
                ),
                remote_signed_prekey_id: bootstrap.remote_signed_prekey_id,
                remote_one_time_prekey_id: bootstrap.remote_one_time_prekey_id,
                bootstrap_signature_r: signature_to_wire(bootstrap.bootstrap_signature.clone()).0,
                bootstrap_signature_s: signature_to_wire(bootstrap.bootstrap_signature.clone()).1,
                created_at_unix_ms: bootstrap.created_at_unix_ms,
                initiator_instance_id: self.storage.get_or_create_local_instance_id()?,
                initiator_device_certificate_chain_json: local_device_certificate_chain_json(
                    &self.storage,
                )?,
            };

            let envelope = omni_messaging::wrap_session_bootstrap(bootstrap_v1)?;
            self.transport.send_wire_envelope(&envelope)?;
        }
        if !self.wait_briefly_for_established_session_for_remote_account(&remote_account)? {
            return Ok(TransportCommandResult {
                ok: true,
                message: format!(
                    "contact request bootstrap sent for {remote_account_id}; waiting for the secure session to finish"
                ),
                snapshot: self.transport.snapshot(),
            });
        }

        Ok(TransportCommandResult {
            ok: true,
            message: format!("contact request refreshed for {remote_account_id}"),
            snapshot: self.transport.snapshot(),
        })
    }
}
