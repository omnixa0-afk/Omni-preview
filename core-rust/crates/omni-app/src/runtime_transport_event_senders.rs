// Outbound transport signal senders: public-profile sync (descriptor+avatar), contact
// approval/request signals, route-hints sync, and their session/active-transport entry points.
// include!d into runtime_transport_events.rs - same module scope + imports (no `use` here by
// design, mirroring runtime_transport_event_helpers.rs). Split for size; identical behaviour.

fn send_current_public_profile_sync_for_session(
    storage: &Storage,
    transport: &TransportManager,
    session: &omni_crypto::SessionRecord,
    target_dialog_key: Option<&str>,
) -> OmniResult<()> {
    if session.bootstrap_state != omni_crypto::SessionBootstrapState::Established {
        return Ok(());
    }

    let updated_session = send_current_public_profile_descriptor_sync_for_session(
        storage,
        transport,
        session,
        target_dialog_key,
    )?;
    send_current_public_profile_avatar_sync_for_session(
        storage,
        transport,
        &updated_session,
        target_dialog_key,
    )?;
    Ok(())
}

fn clear_public_profile_sync_registry_for_session(
    db_path: &str,
    session: &omni_crypto::SessionRecord,
) -> OmniResult<()> {
    let registry_key = public_profile_sync_registry_key(
        db_path,
        &session.session_address.remote_account_id,
        &session.session_address.remote_device_id,
    );
    lock_public_profile_sync_registry()?.remove(&registry_key);
    Ok(())
}

fn force_send_current_public_profile_sync_for_session(
    storage: &Storage,
    transport: &TransportManager,
    session: &omni_crypto::SessionRecord,
    target_dialog_key: Option<&str>,
) -> OmniResult<()> {
    clear_public_profile_sync_registry_for_session(&storage.db_path().to_string_lossy(), session)?;
    send_current_public_profile_sync_for_session(storage, transport, session, target_dialog_key)
}

fn send_current_public_profile_descriptor_sync_for_session(
    storage: &Storage,
    transport: &TransportManager,
    session: &omni_crypto::SessionRecord,
    target_dialog_key: Option<&str>,
) -> OmniResult<omni_crypto::SessionRecord> {
    let state = storage.get_local_public_profile_state()?;
    let registry_key = public_profile_sync_registry_key(
        &storage.db_path().to_string_lossy(),
        &session.session_address.remote_account_id,
        &session.session_address.remote_device_id,
    );
    {
        let registry = lock_public_profile_sync_registry()?;
        if !should_send_public_profile_descriptor_sync(
            registry.get(&registry_key),
            state.updated_at_unix_ms,
            session.created_at_unix_ms,
        ) {
            return Ok(session.clone());
        }
    }

    let mut outgoing = omni_messaging::build_encrypted_logical_event_envelope(
        session,
        &omni_types::LogicalEventEnvelopeV1::public_profile_descriptor_sync(
            PUBLIC_PROFILE_DESCRIPTOR_SYNC_VERSION_V1,
            state.display_name,
            state.avatar_hash_hex.clone(),
            state.avatar_mime_type.clone(),
            state.updated_at_unix_ms,
        ),
        SecurityPayloadKind::PublicProfileDescriptorSync,
        current_unix_ms(),
    )?;
    storage.store_session_record(&mut outgoing.updated_session)?;
    if let Some(dialog_key) = target_dialog_key {
        transport.send_wire_envelope_for_dialog_key(dialog_key, &outgoing.envelope)?;
    } else {
        transport.send_wire_envelope(&outgoing.envelope)?;
    }

    let mut registry = lock_public_profile_sync_registry()?;
    let entry = registry
        .entry(registry_key)
        .or_insert_with(PublicProfileSyncRegistryEntry::default);
    entry.descriptor_updated_at_unix_ms = state.updated_at_unix_ms;
    entry.descriptor_session_created_at_unix_ms = Some(session.created_at_unix_ms);
    if state.avatar_hash_hex.is_none() {
        entry.avatar_hash_hex = None;
        entry.avatar_session_created_at_unix_ms = None;
        entry.avatar_last_attempt_at_unix_ms = None;
        entry.avatar_attempt_count = 0;
    }
    Ok(outgoing.updated_session)
}

fn send_current_public_profile_avatar_sync_for_session(
    storage: &Storage,
    transport: &TransportManager,
    session: &omni_crypto::SessionRecord,
    target_dialog_key: Option<&str>,
) -> OmniResult<omni_crypto::SessionRecord> {
    let state = storage.get_local_public_profile_state()?;
    let registry_key = public_profile_sync_registry_key(
        &storage.db_path().to_string_lossy(),
        &session.session_address.remote_account_id,
        &session.session_address.remote_device_id,
    );
    let Some(avatar_bytes) = state.avatar_bytes.as_ref() else {
        return Ok(session.clone());
    };
    let Some(avatar_hash_hex) = state.avatar_hash_hex.as_deref() else {
        return Ok(session.clone());
    };
    let now_unix_ms = current_unix_ms();
    {
        let mut registry = lock_public_profile_sync_registry()?;
        let entry = registry
            .entry(registry_key.clone())
            .or_insert_with(PublicProfileSyncRegistryEntry::default);
        reset_public_profile_avatar_sync_entry(
            entry,
            Some(avatar_hash_hex),
            session.created_at_unix_ms,
        );
        if !should_send_public_profile_avatar_sync(
            Some(entry),
            avatar_hash_hex,
            session.created_at_unix_ms,
            now_unix_ms,
        ) {
            return Ok(session.clone());
        }
    }

    let mut outgoing = omni_messaging::build_encrypted_logical_event_envelope(
        session,
        &omni_types::LogicalEventEnvelopeV1::public_profile_avatar_sync(
            PUBLIC_PROFILE_AVATAR_SYNC_VERSION_V1,
            state.avatar_hash_hex.clone(),
            state.avatar_mime_type.clone(),
            Some(BASE64_STANDARD.encode(avatar_bytes)),
            state.updated_at_unix_ms,
        ),
        SecurityPayloadKind::PublicProfileAvatarSync,
        now_unix_ms,
    )?;
    storage.store_session_record(&mut outgoing.updated_session)?;
    if let Some(dialog_key) = target_dialog_key {
        transport.send_wire_envelope_for_dialog_key(dialog_key, &outgoing.envelope)?;
    } else {
        transport.send_wire_envelope(&outgoing.envelope)?;
    }

    let mut registry = lock_public_profile_sync_registry()?;
    let entry = registry
        .entry(registry_key)
        .or_insert_with(PublicProfileSyncRegistryEntry::default);
    entry.avatar_hash_hex = Some(avatar_hash_hex.to_string());
    entry.avatar_session_created_at_unix_ms = Some(session.created_at_unix_ms);
    entry.avatar_last_attempt_at_unix_ms = Some(now_unix_ms);
    entry.avatar_attempt_count = entry.avatar_attempt_count.saturating_add(1);
    Ok(outgoing.updated_session)
}

fn active_remote_bundle_for_profile_sync(
    storage: &Storage,
    transport: &TransportManager,
) -> OmniResult<Option<omni_crypto::PrekeyBundle>> {
    if let Some(bundle) = transport.active_remote_prekey_bundle() {
        let domain_bundle = prekey_bundle_from_protocol(&bundle)?;
        storage.record_remote_prekey_bundle(
            &domain_bundle,
            None,
            Some(&bundle.device.device_label),
            bundle.generated_at_unix_ms,
        )?;
        return Ok(Some(domain_bundle));
    }

    let Some(remote_account_id) = transport.active_remote_account_id() else {
        return Ok(None);
    };
    storage.get_latest_remote_prekey_bundle_for_account(&AccountId(remote_account_id))
}

fn active_established_session_for_profile_sync(
    storage: &Storage,
    transport: &TransportManager,
) -> OmniResult<Option<omni_crypto::SessionRecord>> {
    let Some(remote_account_id) = transport.active_remote_account_id().map(AccountId) else {
        return Ok(None);
    };
    let Some(contact) = storage.find_contact_by_remote_account_id(&remote_account_id)? else {
        return Ok(None);
    };
    if contact.relationship_state != ContactRelationshipState::Approved {
        return Ok(None);
    }

    let Some(bundle) = active_remote_bundle_for_profile_sync(storage, transport)? else {
        return Ok(None);
    };
    let session_address = omni_crypto::SessionAddress {
        remote_account_id: bundle.account_id,
        remote_device_id: bundle.device_id,
    };
    let Some(session) = storage.get_session_record(&session_address)? else {
        return Ok(None);
    };
    if session.bootstrap_state != omni_crypto::SessionBootstrapState::Established {
        return Ok(None);
    }
    Ok(Some(session))
}

/// A local profile change (name, avatar) goes to EVERY approved contact we hold an established
/// session with, over that contact's own dialog — the mesh carries it like any other frame. It used
/// to go only to the pre-mesh transport's single "active" connection, which the mesh never fills, so
/// a rename reached a contact only at its next re-link (the on-link force-send). Best-effort per
/// contact; the per-session sync registry still dedups an unchanged profile.
pub(crate) fn send_current_public_profile_sync_to_approved_contacts(
    storage: &Storage,
    transport: &TransportManager,
) -> OmniResult<()> {
    for contact in storage.list_contacts()? {
        if contact.relationship_state != ContactRelationshipState::Approved {
            continue;
        }
        let Some(remote_account_id) = contact.remote_account_id.as_ref() else {
            continue;
        };
        // Every device of the contact shows our profile: each on its own dialog and session.
        for link in storage.list_contact_device_dialogs(&contact.contact_id)? {
            let Some(session) = storage.get_session_for_dialog(remote_account_id, &link.dialog_key)?
            else {
                continue;
            };
            let _ = send_current_public_profile_sync_for_session(
                storage,
                transport,
                &session,
                Some(&link.dialog_key),
            );
        }
    }
    Ok(())
}

pub(crate) fn send_contact_approval_signal_for_session(
    storage: &Storage,
    transport: &TransportManager,
    session: &omni_crypto::SessionRecord,
    target_dialog_key: Option<&str>,
) -> OmniResult<omni_crypto::SessionRecord> {
    let now_unix_ms = current_unix_ms();
    omni_app_trace_log(
        "approval_signal/send",
        &format!(
            "bootstrap_state={:?} target_dialog_key={:?}",
            session.bootstrap_state, target_dialog_key
        ),
    );
    let mut outgoing = omni_messaging::build_encrypted_logical_event_envelope(
        session,
        &omni_types::LogicalEventEnvelopeV1::contact_presence_pong(
            format!("{CONTACT_APPROVAL_SIGNAL_PREFIX}{now_unix_ms}"),
            now_unix_ms,
        ),
        SecurityPayloadKind::ContactPresencePong,
        now_unix_ms,
    )?;
    storage.store_session_record(&mut outgoing.updated_session)?;
    if let Some(dialog_key) = target_dialog_key {
        transport.send_wire_envelope_for_dialog_key(dialog_key, &outgoing.envelope)?;
    } else {
        transport.send_wire_envelope(&outgoing.envelope)?;
    }
    Ok(outgoing.updated_session)
}

pub(crate) fn schedule_contact_approval_signal_retry_for_active_transport(
    storage: &Storage,
    transport_handle: Arc<Mutex<Option<TransportManager>>>,
    session_lock: Arc<Mutex<()>>,
    contact_id: &ContactId,
    dialog_key: &str,
) {
    let db_path = storage.db_path().to_path_buf();
    let contact_id = contact_id.clone();
    let dialog_key = dialog_key.to_string();

    let _ = thread::Builder::new()
        .name("omni-contact-approval-retry".to_string())
        .spawn(move || {
            for delay_ms in CONTACT_APPROVAL_RETRY_DELAYS_MS {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));

                let Ok(_session_guard) = lock_app_session_guard(&session_lock) else {
                    return;
                };
                let Ok(storage) = Storage::open(&db_path) else {
                    continue;
                };
                if !storage.is_profile_signed_in().unwrap_or(false) {
                    return;
                }
                let Ok(Some(transport)) = clone_active_transport(&transport_handle) else {
                    return;
                };
                let Ok(contact) = storage.get_contact(&contact_id) else {
                    return;
                };
                if contact.relationship_state != ContactRelationshipState::Approved {
                    return;
                }
                let mut send_dialog_key = dialog_key.clone();
                let active_snapshot = transport.snapshot();
                if transport.connection_snapshot_for_dialog(&send_dialog_key).is_none()
                    && transport_snapshot_matches_contact(
                        &active_snapshot,
                        &contact,
                        std::slice::from_ref(&dialog_key),
                    )
                {
                    if let Some(active_dialog_key) = active_snapshot.dialog_key.as_ref() {
                        send_dialog_key = active_dialog_key.clone();
                    }
                }

                let Some(connection) = transport.connection_snapshot_for_dialog(&send_dialog_key)
                else {
                    continue;
                };
                let Some(remote_account_id) = contact.remote_account_id.as_ref() else {
                    return;
                };
                if connection
                    .remote_account_id
                    .as_deref()
                    .is_some_and(|value| value != remote_account_id.0.as_str())
                {
                    return;
                }
                let Ok(Some(session)) =
                    storage.get_session_for_dialog(remote_account_id, &send_dialog_key)
                else {
                    continue;
                };
                if session.bootstrap_state != omni_crypto::SessionBootstrapState::Established {
                    continue;
                }

                let db_path_string = storage.db_path().to_string_lossy().to_string();
                let approval_pending =
                    runtime_chat_session_approval_signal_pending(&db_path_string, &dialog_key)
                        .unwrap_or(false)
                        || runtime_chat_session_approval_signal_pending(
                            &db_path_string,
                            &send_dialog_key,
                        )
                        .unwrap_or(false);
                if !approval_pending {
                    return;
                }

                if send_contact_approval_signal_for_session(
                    &storage,
                    &transport,
                    &session,
                    Some(&send_dialog_key),
                )
                .is_ok()
                {
                    let _ = clear_runtime_chat_session_approval_signal_pending(
                        &db_path_string,
                        &dialog_key,
                    );
                    if send_dialog_key != dialog_key {
                        let _ = clear_runtime_chat_session_approval_signal_pending(
                            &db_path_string,
                            &send_dialog_key,
                        );
                    }
                    return;
                }
            }
        });
}

pub(crate) fn send_contact_request_signal_for_transport_session(
    storage: &Storage,
    transport: &TransportManager,
    session: &omni_crypto::SessionRecord,
    target_dialog_key: Option<&str>,
) -> OmniResult<omni_crypto::SessionRecord> {
    let now_unix_ms = current_unix_ms();
    omni_app_trace_log(
        "request_signal/send",
        &format!(
            "site=transport_session bootstrap_state={:?} target_dialog_key={:?}",
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
    storage.store_session_record(&mut outgoing.updated_session)?;
    if let Some(dialog_key) = target_dialog_key {
        transport.send_wire_envelope_for_dialog_key(dialog_key, &outgoing.envelope)?;
        let _ = mark_runtime_chat_session_request_signal_sent(
            &storage.db_path().to_string_lossy(),
            dialog_key,
        );
        omni_app_trace_log(
            "request_signal/sent",
            &format!("via=dialog_key dialog_key={dialog_key}"),
        );
    } else {
        transport.send_wire_envelope(&outgoing.envelope)?;
        omni_app_trace_log("request_signal/sent", "via=wire_envelope dialog_key=none");
    }
    Ok(outgoing.updated_session)
}

pub(crate) fn clear_runtime_contact_ephemera_for_active_profile(
    storage: &Storage,
) -> OmniResult<()> {
    let db_path = storage.db_path().to_string_lossy().to_string();
    lock_verification_registry()?.remove(&db_path);
    let prefix = format!("{db_path}::");
    lock_public_profile_sync_registry()?.retain(|key, _| !key.starts_with(&prefix));
    lock_relay_set_push_registry()?.retain(|key, _| !key.starts_with(&prefix));
    clear_inbound_bootstrap_marks_for_profile(&db_path);
    Ok(())
}

/// R1c relay-set propagation (live-channel PRIMARY path): push MY current signed `om3relay:` bundle to
/// an approved, established session, so a LINKED friend auto-learns my (possibly rotated) relay set
/// without a re-invite. Rate-limited by a per-session high-water of the version already sent, so an
/// unchanged set is never re-pushed; a rotation (version bump) makes the next call send. Mirrors the
/// public-profile sync sender (same ratchet-safe serialized call sites: the queue tick + session-up).
/// The per-pair ntfy beacon stays the FALLBACK for UNLINKED contacts. See r1c-rotatable-white-relay.md §B.
fn send_relay_set_update_for_session(
    storage: &Storage,
    transport: &TransportManager,
    session: &omni_crypto::SessionRecord,
    target_dialog_key: Option<&str>,
) -> OmniResult<()> {
    if session.bootstrap_state != omni_crypto::SessionBootstrapState::Established {
        return Ok(());
    }
    let db_path = storage.db_path().to_string_lossy().to_string();
    // My current signed bundle (the relays I flagged `shared`); None ⇒ I share nothing → no push.
    let Some(token) = build_my_relay_bundle_token(storage, &db_path)? else {
        return Ok(());
    };
    let version = i64::from(load_relay_bundle_version(&db_path));
    let registry_key = relay_set_push_registry_key(
        &db_path,
        &session.session_address.remote_account_id,
        &session.session_address.remote_device_id,
    );
    if lock_relay_set_push_registry()?
        .get(&registry_key)
        .is_some_and(|&sent| sent >= version)
    {
        return Ok(()); // this session already has my current set
    }

    let mut outgoing = omni_messaging::build_encrypted_logical_event_envelope(
        session,
        &omni_types::LogicalEventEnvelopeV1::relay_set_update(omni_types::RelaySetUpdateV1 {
            version: omni_types::RELAY_SET_UPDATE_VERSION_V1,
            bundle_token: token,
            exported_at_unix_ms: current_unix_ms(),
        }),
        SecurityPayloadKind::RelaySetUpdate,
        current_unix_ms(),
    )?;
    storage.store_session_record(&mut outgoing.updated_session)?;
    if let Some(dialog_key) = target_dialog_key {
        transport.send_wire_envelope_for_dialog_key(dialog_key, &outgoing.envelope)?;
    } else {
        transport.send_wire_envelope(&outgoing.envelope)?;
    }
    // Record only after a successful send, so a NotConnected target re-sends once it links.
    lock_relay_set_push_registry()?.insert(registry_key, version);
    Ok(())
}

/// Force a relay-set push regardless of the per-session high-water (clears it first) — used on
/// session-up so a freshly-linked friend gets my current set immediately.
fn force_send_relay_set_update_for_session(
    storage: &Storage,
    transport: &TransportManager,
    session: &omni_crypto::SessionRecord,
    target_dialog_key: Option<&str>,
) -> OmniResult<()> {
    {
        let key = relay_set_push_registry_key(
            &storage.db_path().to_string_lossy(),
            &session.session_address.remote_account_id,
            &session.session_address.remote_device_id,
        );
        lock_relay_set_push_registry()?.remove(&key);
    }
    send_relay_set_update_for_session(storage, transport, session, target_dialog_key)
}

/// Tell the contact on this dialog whether we let it control this computer
/// (docs/decisions/remote-access-for-contacts.md). Called on change and again on every link, so a
/// revocation made while the contact was offline still reaches it. `sent_at` is when OUR side last
/// changed, not now: the receiver keeps only a newer stamp, so a re-send is a no-op there and can
/// never overtake a later change. A contact we never granted hears nothing.
fn send_remote_access_grant_for_session(
    storage: &Storage,
    transport: &TransportManager,
    session: &omni_crypto::SessionRecord,
    dialog_key: &str,
) -> OmniResult<()> {
    if session.bootstrap_state != omni_crypto::SessionBootstrapState::Established {
        return Ok(());
    }
    let Some(contact) = storage.resolve_contact_for_dialog(dialog_key)? else {
        return Ok(());
    };
    if contact.relationship_state != ContactRelationshipState::Approved {
        return Ok(());
    }
    let access = storage.get_contact_remote_access(&contact.contact_id)?;
    let Some(changed_at_unix_ms) = access.granted_changed_at_unix_ms else {
        return Ok(());
    };
    let mut outgoing = omni_messaging::build_encrypted_logical_event_envelope(
        session,
        &omni_types::LogicalEventEnvelopeV1::remote_access_grant(access.granted_mode, changed_at_unix_ms),
        SecurityPayloadKind::RemoteAccessGrant,
        current_unix_ms(),
    )?;
    storage.store_session_record(&mut outgoing.updated_session)?;
    transport.send_wire_envelope_for_dialog_key(dialog_key, &outgoing.envelope)?;
    Ok(())
}
