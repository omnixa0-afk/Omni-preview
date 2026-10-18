#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatHeaderScreenSnapshot {
    pub header_summary: ChatHeaderContactSummary,
    pub security_summary: ChatSecuritySummary,
    pub session_summary: omni_types::ChatSessionSummary,
    pub call_summary: Option<CallSummary>,
}

const SESSION_END_DISCONNECT_GRACE: std::time::Duration = std::time::Duration::from_millis(750);

impl OmniApp {
    pub fn get_chat_thread_list_refresh_summary(
        &self,
    ) -> OmniResult<omni_types::ChatThreadListRefreshSummary> {
        let _ = self.ensure_runtime_profile_active("get_chat_thread_list_refresh_summary")?;
        self.storage.get_chat_thread_list_refresh_summary()
    }

    pub fn list_chat_thread_projections(&self) -> OmniResult<Vec<ChatThreadProjection>> {
        let _ = self.ensure_runtime_profile_active("list_chat_thread_projections")?;
        let latest_messages_by_dialog = self.storage.list_latest_messages_by_dialog()?;
        let mut items = Vec::new();

        for contact in self.storage.list_contacts()? {
            if contact.removed_at_unix_ms.is_some() {
                continue;
            }
            if contact.relationship_state != omni_identity::ContactRelationshipState::Approved {
                continue;
            }

            let Some(dialog_key) = contact
                .primary_dialog_key
                .clone()
                .or_else(|| contact.remote_instance_id.as_deref().map(dialog_key_from_instance_id))
                .or_else(|| {
                    contact
                        .remote_account_id
                        .as_ref()
                        .map(|value| omni_types::dialog_key_from_account_id(&value.0))
                })
            else {
                continue;
            };

            let last_message = latest_messages_by_dialog.get(&dialog_key).cloned();
            let unread_count = match &last_message {
                Some(_) => self.storage.count_unread_in_dialog(&dialog_key)?,
                None => 0,
            };
            items.push(ChatThreadProjection {
                contact,
                last_message,
                unread_count,
            });
        }

        Ok(items)
    }

    /// The chat is on screen: everything in it so far is read. 1:1 and group alike.
    pub fn mark_chat_read(&self, dialog_key_or_selector: impl Into<String>) -> OmniResult<()> {
        let _ = self.ensure_runtime_profile_active("mark_chat_read")?;
        // The key the chat's lines are stored under, as `insert_incoming_message` files them.
        let dialog_key = self.storage.chat_dialog_key_for(dialog_key_or_selector.into().trim())?;
        self.storage.mark_dialog_read(&dialog_key)
    }

    pub fn resolve_contact_for_dialog(
        &self,
        dialog_key_or_selector: impl Into<String>,
    ) -> OmniResult<Option<ContactCardSnapshot>> {
        let dialog_key = resolve_canonical_dialog_key_for_selector(
            &self.storage,
            dialog_key_or_selector.into().trim(),
        )?;
        self.storage.resolve_contact_card_for_dialog(&dialog_key)
    }

    pub fn get_chat_header_contact_summary(
        &self,
        dialog_key_or_selector: impl Into<String>,
    ) -> OmniResult<ChatHeaderContactSummary> {
        let selector = dialog_key_or_selector.into();
        let dialog_key = resolve_canonical_dialog_key_for_selector(&self.storage, selector.trim())?;
        let contact_card = self.storage.resolve_contact_card_for_dialog(&dialog_key)?;
        let (title, subtitle) = if let Some(card) = &contact_card {
            let base_label = card
                .contact
                .public_display_name
                .clone()
                .or_else(|| {
                    card.contact
                        .remote_account_id
                        .as_ref()
                        .map(|value| value.0.clone())
                })
                .unwrap_or_else(|| dialog_key.clone());
            // The header title is the petname; when the public display name equals it, prefixing
            // the subtitle with the same name just repeats the title (the name would show twice).
            // Keep only the status part in that case.
            let name_prefix = if base_label == card.contact.petname {
                String::new()
            } else {
                format!("{base_label} • ")
            };
            let subtitle = if matches!(
                card.contact.relationship_state,
                omni_identity::ContactRelationshipState::Approved
            ) && omni_types::account_id_from_dialog_key(&dialog_key).is_some()
                && card.contact.remote_instance_id.is_none()
            {
                format!("{name_prefix}Контакт сохранён, ждём первое подключение")
            } else if matches!(
                card.contact.relationship_state,
                omni_identity::ContactRelationshipState::Approved
            ) {
                // Присутствие здесь БОЛЬШЕ НЕ ПИШЕМ. Раньше в подпись подставлялась метка,
                // выведенная из ОБЪЯВЛЕННОГО присутствия контакта, — а точку рядом оболочка
                // рисовала из живого линка. Два источника, два такта обновления, и на экране
                // «В сети» соседствовало с оранжевой точкой (замечено пользователем 2026-08-14).
                //
                // Достижимость — свойство сессии, а не поле контакта
                // (docs/decisions/reachability-belongs-to-the-session.md). Слово и цвет теперь
                // обе рисует оболочка из ОДНОГО решения (`ContactPresenceDotKind`), посчитанного
                // раз за отрисовку, — разойтись им нечем.
                let base = name_prefix.trim_end_matches(" • ").to_string();
                // A chat that came back from the contact's copy after a restore and did not match
                // what the passport counted says so — softly, in the header, never in the way.
                let note = card
                    .contact
                    .remote_account_id
                    .as_ref()
                    .and_then(|account| self.storage.get_chat_history_check(account).ok().flatten())
                    .and_then(|check| match check {
                        omni_storage::ChatHistoryCheck::Verified => None,
                        omni_storage::ChatHistoryCheck::Incomplete => Some("история восстановлена не полностью"),
                        omni_storage::ChatHistoryCheck::Mismatch => Some("история не сходится с сохранённой"),
                    });
                match note {
                    Some(note) if base.is_empty() => note.to_string(),
                    Some(note) => format!("{base} • {note}"),
                    None => base,
                }
            } else if base_label == card.contact.petname {
                String::new()
            } else {
                base_label
            };
            (card.contact.petname.clone(), subtitle)
        } else if let Some(instance_id) = instance_id_from_dialog_key(&dialog_key) {
            ("Новый диалог".to_string(), format!("Диалог с узлом {instance_id}"))
        } else if let Some(account_id) = omni_types::account_id_from_dialog_key(&dialog_key) {
            (
                "Новый диалог".to_string(),
                format!("Контакт по account id {account_id}"),
            )
        } else {
            ("Новый диалог".to_string(), dialog_key.clone())
        };

        Ok(ChatHeaderContactSummary {
            dialog_key,
            title,
            subtitle,
            contact_card,
        })
    }

    pub fn get_chat_header_screen_snapshot(
        &self,
        dialog_key_or_selector: impl Into<String>,
    ) -> OmniResult<ChatHeaderScreenSnapshot> {
        let header_summary = self.get_chat_header_contact_summary(dialog_key_or_selector.into())?;
        let security_summary = self.get_chat_security_summary(&header_summary.dialog_key)?;
        let session_summary =
            build_chat_session_summary(
                &self.storage,
                &self.transport,
                &header_summary.dialog_key,
                self.peer_link_facts_for_dialog(&header_summary.dialog_key),
            )?;
        let call_summary =
            current_or_persisted_call_summary_for_dialog(&self.storage, &header_summary.dialog_key)?;

        Ok(ChatHeaderScreenSnapshot {
            header_summary,
            security_summary,
            session_summary,
            call_summary,
        })
    }

    pub fn open_chat_session_lease(
        &self,
        dialog_key_or_selector: impl Into<String>,
    ) -> OmniResult<omni_types::ChatSessionSummary> {
        let _ = self.ensure_runtime_profile_active("open_chat_session_lease")?;
        let dialog_key =
            open_chat_session_lease_for_profile(&self.storage, dialog_key_or_selector.into().trim())?;
        build_chat_session_summary(
            &self.storage,
            &self.transport,
            &dialog_key,
            self.peer_link_facts_for_dialog(&dialog_key),
        )
    }

    pub fn end_chat_session(
        &self,
        dialog_key_or_selector: impl Into<String>,
    ) -> OmniResult<omni_types::ChatSessionSummary> {
        let _ = self.ensure_runtime_profile_active("end_chat_session")?;
        let dialog_key = pause_chat_session_for_profile(&self.storage, dialog_key_or_selector.into().trim())?;
        let now_unix_ms = current_unix_ms();
        let _ = mark_contact_offline_for_dialog(&self.storage, &dialog_key, now_unix_ms);
        let mut transport_dialog_key = dialog_key.clone();
        let mut connection = self.transport.connection_snapshot_for_dialog(&transport_dialog_key);
        if connection.is_none() {
            if let Some(contact) = self.storage.resolve_contact_for_dialog(&dialog_key)? {
                if self.active_transport_matches_contact(&contact) {
                    if let Some(active_dialog_key) = self.transport.active_dialog_key() {
                        transport_dialog_key = active_dialog_key;
                        connection = self
                            .transport
                            .connection_snapshot_for_dialog(&transport_dialog_key);
                    }
                }
            }
        }
        if let Some(connection) = connection {
            let mut sent_pause_notice = false;
            // The notice is encrypted with the pair's ratchet session, so it is read, stored and sent
            // under the session lock like every other frame. Without it a receipt arriving from the
            // peer in the same moment stored a newer copy first, our store was refused as stale,
            // the notice was never sent and the peer's lease stayed open — the flaky
            // `ending_chat_session_pauses_the_remote_open_lease` (docs/architecture/test-patterns.md).
            let session_guard = lock_app_session_guard(&self.session_lock)?;
            if let (Some(remote_account_id), Some(remote_device_id)) = (
                connection.remote_account_id.as_deref(),
                connection.remote_device_id.as_deref(),
            ) {
                let payload = omni_types::WirePayloadV1::LogicalEvent(
                    omni_types::LogicalEventEnvelopeV1::pause_auto_reconnect(now_unix_ms),
                );
                if let Ok(mut outgoing) = prepare_encrypted_payload_for_session_address(
                    &self.storage,
                    &omni_crypto::SessionAddress {
                        remote_account_id: AccountId(remote_account_id.to_string()),
                        remote_device_id: DeviceId(remote_device_id.to_string()),
                    },
                    &payload,
                    SecurityPayloadKind::PauseAutoReconnect,
                ) {
                    sent_pause_notice = self
                        .storage
                        .store_session_record(&mut outgoing.updated_session)
                        .is_ok()
                        && self
                            .transport
                            .send_wire_envelope_for_dialog_key(
                                &transport_dialog_key,
                                &outgoing.envelope,
                            )
                            .is_ok();
                }
            }
            drop(session_guard);
            if !sent_pause_notice {
                omni_app_trace_log(
                    "chat/session-end",
                    &format!("dialog_key={transport_dialog_key} pause notice not sent; disconnecting now"),
                );
            }
            if sent_pause_notice {
                disconnect_dialog_after_session_end_notice(
                    self.transport.clone(),
                    transport_dialog_key,
                );
            } else {
                let _ = self.transport.disconnect_dialog(&transport_dialog_key);
            }
        }
        let _ = touch_runtime_chat_session_interaction(
            &self.storage.db_path().to_string_lossy(),
            &dialog_key,
            omni_types::ChatSessionInteractionKind::Session,
            now_unix_ms,
        );
        build_chat_session_summary(
            &self.storage,
            &self.transport,
            &dialog_key,
            self.peer_link_facts_for_dialog(&dialog_key),
        )
    }

    pub fn start_chat_from_contact(&self, contact_id: &ContactId) -> OmniResult<ContactChatTarget> {
        let _ = self.ensure_runtime_profile_active("start_chat_from_contact")?;
        let mut contact = self.storage.get_contact(contact_id)?;
        if contact.relationship_state != omni_identity::ContactRelationshipState::Approved {
            return Err(OmniError::InvalidInput(
                "contact must be approved before opening chat".to_string(),
            ));
        }
        let dialog_key = if let Some(primary_dialog_key) = contact.primary_dialog_key.clone() {
            primary_dialog_key
        } else if let Some(remote_instance_id) = contact.remote_instance_id.as_deref() {
            let dialog_key = dialog_key_from_instance_id(remote_instance_id);
            contact = self
                .storage
                .link_dialog_to_contact(contact_id, &dialog_key, true, current_unix_ms())?;
            dialog_key
        } else if let Some(remote_account_id) = contact.remote_account_id.as_ref() {
            let dialog_key = omni_types::dialog_key_from_account_id(&remote_account_id.0);
            contact = self
                .storage
                .link_dialog_to_contact(contact_id, &dialog_key, true, current_unix_ms())?;
            dialog_key
        } else {
            return Err(OmniError::InvalidInput(
                "contact is not linked to a dialog yet".to_string(),
            ));
        };

        Ok(ContactChatTarget {
            contact_id: contact.contact_id,
            dialog_key,
            title: contact.petname,
        })
    }

    pub fn quick_add_contact_from_dialog(
        &self,
        dialog_key_or_selector: impl Into<String>,
        petname: impl Into<String>,
        public_display_name: Option<String>,
    ) -> OmniResult<ContactSnapshot> {
        let _ = self.ensure_runtime_profile_active("quick_add_contact_from_dialog")?;
        let selector = dialog_key_or_selector.into();
        let dialog_key = resolve_canonical_dialog_key_for_selector(&self.storage, selector.trim())?;
        let active_snapshot = self.transport.snapshot();
        let selector_matches_active = selector_matches_snapshot(selector.trim(), &active_snapshot)
            || active_snapshot.dialog_key.as_deref() == Some(dialog_key.as_str());
        let remote_instance_id = active_snapshot
            .dialog_key
            .as_deref()
            .filter(|active| *active == dialog_key)
            .and_then(|_| active_snapshot.remote_instance_id.clone())
            .or_else(|| instance_id_from_dialog_key(&dialog_key));

        let display_name = public_display_name
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .or_else(|| {
                selector_matches_active
                    .then(|| active_snapshot.remote_device_name.clone())
                    .flatten()
            });
        let remote_account_id = active_snapshot
            .dialog_key
            .as_deref()
            .filter(|active| selector_matches_active && *active == dialog_key)
            .and_then(|_| self.transport.active_remote_account_id())
            .map(omni_identity::AccountId);
        let existing = self.storage.resolve_contact_for_dialog(&dialog_key)?;
        let petname = match existing.as_ref() {
            Some(contact) => contact.petname.clone(),
            None => petname.into(),
        };

        let contact = self.add_contact(&AddContactInput {
            petname,
            remote_account_id,
            public_display_name: display_name,
            remote_instance_id,
            primary_dialog_key: Some(dialog_key),
            initial_observation: None,
        })?;

        if let Ok(verified) = self.verify_contact(
            &contact.contact_id,
            omni_identity::VerificationMethod::ChallengeResponse,
        ) {
            return Ok(verified.contact);
        }

        self.storage.get_contact(&contact.contact_id)
    }

    pub fn attach_dialog_to_contact(
        &self,
        contact_id: &ContactId,
        dialog_key_or_selector: impl Into<String>,
    ) -> OmniResult<ContactSnapshot> {
        let _ = self.ensure_runtime_profile_active("attach_dialog_to_contact")?;
        let dialog_key = normalize_dialog_key(dialog_key_or_selector.into().trim());
        let contact = self.storage.get_contact(contact_id)?;
        if contact.relationship_state != omni_identity::ContactRelationshipState::Approved {
            return Err(OmniError::InvalidInput(
                "contact must be approved before linking a dialog".to_string(),
            ));
        }
        self.storage
            .link_dialog_to_contact(contact_id, &dialog_key, true, current_unix_ms())
    }
}

fn disconnect_dialog_after_session_end_notice(transport: TransportManager, dialog_key: String) {
    thread::spawn(move || {
        thread::sleep(SESSION_END_DISCONNECT_GRACE);
        let _ = transport.disconnect_dialog(&dialog_key);
    });
}

// `contact_presence_label` удалён вместе с его единственным вызовом: он выводил присутствие из
// объявленного состояния контакта, ничего не зная о живом линке, и потому противоречил точке,
// которую оболочка рисует из линка. Теперь присутствие рендерит только оболочка, из одного решения.
// См. docs/bugs/presence-dot-frozen-by-its-own-cache.md.
