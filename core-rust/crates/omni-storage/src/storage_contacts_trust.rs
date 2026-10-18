impl Storage {
    pub fn add_contact(
        &self,
        input: &AddContactInput,
        now_unix_ms: i64,
    ) -> OmniResult<ContactSnapshot> {
        self.add_contact_with_relationship_state(
            input,
            now_unix_ms,
            omni_identity::ContactRelationshipState::Approved,
            true,
            None,
        )
    }

    pub fn add_pending_contact(
        &self,
        input: &AddContactInput,
        now_unix_ms: i64,
    ) -> OmniResult<ContactSnapshot> {
        self.add_pending_contact_with_direction(
            input,
            now_unix_ms,
            Some(ContactRequestDirection::Incoming),
        )
    }

    pub fn add_outgoing_pending_contact(
        &self,
        input: &AddContactInput,
        now_unix_ms: i64,
    ) -> OmniResult<ContactSnapshot> {
        self.add_pending_contact_with_direction(
            input,
            now_unix_ms,
            Some(ContactRequestDirection::Outgoing),
        )
    }

    pub fn add_pending_contact_with_direction(
        &self,
        input: &AddContactInput,
        now_unix_ms: i64,
        pending_request_direction: Option<ContactRequestDirection>,
    ) -> OmniResult<ContactSnapshot> {
        self.add_contact_with_relationship_state(
            input,
            now_unix_ms,
            omni_identity::ContactRelationshipState::Pending,
            false,
            pending_request_direction,
        )
    }

    fn add_contact_with_relationship_state(
        &self,
        input: &AddContactInput,
        now_unix_ms: i64,
        relationship_state: omni_identity::ContactRelationshipState,
        promote_existing_to_requested_state: bool,
        pending_request_direction: Option<ContactRequestDirection>,
    ) -> OmniResult<ContactSnapshot> {
        fn default_primary_dialog_key(
            input: &AddContactInput,
            relationship_state: omni_identity::ContactRelationshipState,
        ) -> Option<String> {
            if let Some(primary_dialog_key) = input.primary_dialog_key.as_deref() {
                return Some(omni_types::normalize_dialog_key(primary_dialog_key));
            }

            if relationship_state == omni_identity::ContactRelationshipState::Approved
                && input.remote_instance_id.is_none()
            {
                return input
                    .remote_account_id
                    .as_ref()
                    .map(|value| omni_types::dialog_key_from_account_id(&value.0));
            }

            None
        }

        validate_add_contact_input(input)?;
        let profile_id = self.require_active_profile_id()?;
        let requested_primary_dialog_key = default_primary_dialog_key(input, relationship_state);

        if let Some(existing) = self.resolve_existing_contact_for_add(input, now_unix_ms)? {
            let mut should_refresh_contact = false;
            let remote_account_id_hint = existing
                .remote_account_id
                .as_ref()
                .map(|value| value.0.as_str())
                .or_else(|| input.remote_account_id.as_ref().map(|value| value.0.as_str()));

            let mut petname = existing.petname.clone();
            let incoming_petname = input.petname.trim();
            if !incoming_petname.is_empty()
                && name_looks_like_account_placeholder(&existing.petname, remote_account_id_hint)
            {
                petname = incoming_petname.to_string();
                should_refresh_contact = true;
            }

            let mut remote_account_id = existing.remote_account_id.clone();
            if remote_account_id.is_none() {
                if let Some(value) = input.remote_account_id.as_ref() {
                    remote_account_id = Some(value.clone());
                    should_refresh_contact = true;
                }
            }

            let mut public_display_name = existing.public_display_name.clone();
            if let Some(value) = input.public_display_name.as_ref() {
                let trimmed_value = value.trim();
                if !trimmed_value.is_empty()
                    && (public_display_name.is_none()
                        || name_looks_like_account_placeholder(
                            public_display_name.as_deref().unwrap_or_default(),
                            remote_account_id_hint,
                        ))
                {
                    public_display_name = Some(trimmed_value.to_string());
                    should_refresh_contact = true;
                }
            }

            let mut remote_instance_id = existing.remote_instance_id.clone();
            if remote_instance_id.is_none() {
                if let Some(value) = input.remote_instance_id.as_deref() {
                    remote_instance_id = Some(value.to_string());
                    should_refresh_contact = true;
                }
            }

            let mut merged_pending_request_direction = existing.pending_request_direction;
            if existing.relationship_state == omni_identity::ContactRelationshipState::Pending
                && relationship_state == omni_identity::ContactRelationshipState::Pending
            {
                let next_pending_request_direction = merge_pending_request_direction(
                    merged_pending_request_direction,
                    pending_request_direction,
                );
                if next_pending_request_direction != merged_pending_request_direction {
                    merged_pending_request_direction = next_pending_request_direction;
                    should_refresh_contact = true;
                }
            } else if merged_pending_request_direction.is_some() {
                merged_pending_request_direction = None;
                should_refresh_contact = true;
            }

            if should_refresh_contact {
                self.connection
                    .execute(
                        "
                        UPDATE contacts
                        SET petname = ?2,
                            remote_account_id = COALESCE(remote_account_id, ?3),
                            public_display_name = COALESCE(?4, public_display_name),
                            remote_instance_id = COALESCE(remote_instance_id, ?5),
                            pending_request_direction = ?6,
                            updated_at_unix_ms = ?7
                        WHERE contact_id = ?1
                          AND profile_id = ?8
                        ",
                        params![
                            existing.contact_id.0,
                            petname,
                            remote_account_id.as_ref().map(|value| value.0.clone()),
                            public_display_name,
                            remote_instance_id,
                            merged_pending_request_direction.map(|value| value.as_db_value()),
                            now_unix_ms,
                            profile_id.0,
                        ],
                    )
                    .map_err(map_sqlite_error)?;
            }

            if let Some(primary_dialog_key) = requested_primary_dialog_key.as_deref() {
                if existing.primary_dialog_key.as_deref() != Some(primary_dialog_key) {
                    // An existing contact keeps its chat: another dialog of it (another device —
                    // docs/decisions/multi-device-equal-nodes.md) joins as a secondary link.
                    self.upsert_contact_dialog_link(
                        &existing.contact_id,
                        primary_dialog_key,
                        existing.primary_dialog_key.is_none(),
                        now_unix_ms,
                    )?;
                }
            }

            if let Some(observation) = &input.initial_observation {
                self.record_contact_observation(&existing.contact_id, observation)?;
            }

            if promote_existing_to_requested_state
                && existing.relationship_state != relationship_state
            {
                return self.set_contact_relationship_state(
                    &existing.contact_id,
                    relationship_state,
                    now_unix_ms,
                );
            }

            return self.get_contact(&existing.contact_id);
        }

        let contact_id = create_contact_id();
        let snapshot = build_contact_snapshot(
            contact_id.clone(),
            input.petname.trim().to_string(),
            input.remote_account_id.clone(),
            input.public_display_name.clone(),
            None,
            None,
            None,
            None,
            None,
            None,
            input.remote_instance_id.clone(),
            requested_primary_dialog_key,
            relationship_state,
            pending_request_direction,
            false,
            false,
            omni_identity::ContactPresenceState::Offline,
            None,
            None,
            None,
            None,
            now_unix_ms,
            now_unix_ms,
        );

        self.connection
            .execute(
                "
                INSERT INTO contacts (
                    contact_id,
                    petname,
                    remote_account_id,
                    public_display_name,
                    remote_instance_id,
                    relationship_state,
                    pending_request_direction,
                    has_met,
                    is_blocked,
                    removed_at_unix_ms,
                    created_at_unix_ms,
                    updated_at_unix_ms,
                    profile_id
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 0, NULL, ?8, ?9, ?10)
                ",
                params![
                    snapshot.contact_id.0,
                    snapshot.petname,
                    snapshot
                        .remote_account_id
                        .as_ref()
                        .map(|value| value.0.clone()),
                    snapshot.public_display_name,
                    snapshot.remote_instance_id,
                    snapshot.relationship_state.as_db_value(),
                    snapshot
                        .pending_request_direction
                        .map(|value| value.as_db_value()),
                    snapshot.created_at_unix_ms,
                    snapshot.updated_at_unix_ms,
                    profile_id.0,
                ],
            )
            .map_err(map_sqlite_error)?;

        if let Some(primary_dialog_key) = snapshot.primary_dialog_key.as_deref() {
            self.upsert_contact_dialog_link(
                &snapshot.contact_id,
                primary_dialog_key,
                true,
                now_unix_ms,
            )?;
        }

        if let Some(observation) = &input.initial_observation {
            self.record_contact_observation(&snapshot.contact_id, observation)?;
        }

        self.get_contact(&snapshot.contact_id)
    }

    fn resolve_existing_contact_for_add(
        &self,
        input: &AddContactInput,
        now_unix_ms: i64,
    ) -> OmniResult<Option<ContactSnapshot>> {
        let profile_id = self.require_active_profile_id()?;
        let mut candidates = Vec::new();

        if let Some(primary_dialog_key) = input.primary_dialog_key.as_deref() {
            candidates.extend(
                self.find_contacts_for_dialog_in_profile(&profile_id, primary_dialog_key)?,
            );
        }

        if let Some(remote_account_id) = input.remote_account_id.as_ref() {
            candidates.extend(
                self.find_contacts_by_remote_account_id_in_profile(&profile_id, remote_account_id)?,
            );
        }

        if let Some(remote_instance_id) = input.remote_instance_id.as_deref() {
            candidates.extend(
                self.find_contacts_by_remote_instance_id_in_profile(
                    &profile_id,
                    remote_instance_id,
                )?,
            );
        }

        self.merge_candidate_contacts_for_profile(&profile_id, candidates, now_unix_ms)
    }

    pub fn rename_contact(
        &self,
        contact_id: &ContactId,
        petname: &str,
        now_unix_ms: i64,
    ) -> OmniResult<ContactSnapshot> {
        let profile_id = self.require_active_profile_id()?;
        let contact_id = self.resolve_canonical_contact_id_for_profile(
            &profile_id,
            contact_id,
            now_unix_ms,
        )?;
        let petname = petname.trim();
        if petname.is_empty() {
            return Err(OmniError::InvalidInput(
                "petname must not be empty".to_string(),
            ));
        }

        let updated_rows = self
            .connection
            .execute(
                "
                UPDATE contacts
                SET petname = ?2,
                    updated_at_unix_ms = ?3
                WHERE contact_id = ?1
                  AND profile_id = ?4
                ",
                params![contact_id.0, petname, now_unix_ms, profile_id.0],
            )
            .map_err(map_sqlite_error)?;

        if updated_rows == 0 {
            return Err(OmniError::Storage(format!(
                "contact '{}' was not found",
                contact_id.0
            )));
        }

        self.get_contact(&contact_id)
    }

    pub fn set_contact_relationship_state(
        &self,
        contact_id: &ContactId,
        relationship_state: omni_identity::ContactRelationshipState,
        now_unix_ms: i64,
    ) -> OmniResult<ContactSnapshot> {
        let profile_id = self.require_active_profile_id()?;
        let contact_id = self.resolve_canonical_contact_id_for_profile(
            &profile_id,
            contact_id,
            now_unix_ms,
        )?;
        let updated_rows = self
            .connection
            .execute(
                "
                UPDATE contacts
                SET relationship_state = ?2,
                    is_blocked = ?3,
                    removed_at_unix_ms = NULL,
                    pending_request_direction = CASE
                        WHEN ?2 = 'pending' THEN pending_request_direction
                        ELSE NULL
                    END,
                    updated_at_unix_ms = ?4
                WHERE contact_id = ?1
                  AND profile_id = ?5
                ",
                params![
                    contact_id.0,
                    relationship_state.as_db_value(),
                    bool_to_sqlite_integer(matches!(
                        relationship_state,
                        omni_identity::ContactRelationshipState::Declined
                    )),
                    now_unix_ms,
                    profile_id.0
                ],
            )
            .map_err(map_sqlite_error)?;

        if updated_rows == 0 {
            return Err(OmniError::Storage(format!(
                "contact '{}' was not found",
                contact_id.0
            )));
        }

        self.get_contact(&contact_id)
    }

    pub fn approve_contact(
        &self,
        contact_id: &ContactId,
        now_unix_ms: i64,
    ) -> OmniResult<ContactSnapshot> {
        self.set_contact_relationship_state(
            contact_id,
            omni_identity::ContactRelationshipState::Approved,
            now_unix_ms,
        )
    }

    pub fn decline_contact(
        &self,
        contact_id: &ContactId,
        now_unix_ms: i64,
    ) -> OmniResult<ContactSnapshot> {
        self.set_contact_relationship_state(
            contact_id,
            omni_identity::ContactRelationshipState::Declined,
            now_unix_ms,
        )
    }

    /// Friend-proxy (Axis 2): persist per-contact consent to carry this contact's mesh traffic
    /// through us. Off by default, revocable (`consent = false`). The app layer mirrors the change
    /// into the running mesh; this row is the durable source of truth reseeded on mesh start.
    /// See docs/decisions/friend-proxy-by-consent.md (Phase C).
    pub fn set_contact_friend_relay_consent(
        &self,
        contact_id: &ContactId,
        consent: bool,
        now_unix_ms: i64,
    ) -> OmniResult<ContactSnapshot> {
        let profile_id = self.require_active_profile_id()?;
        let contact_id = self.resolve_canonical_contact_id_for_profile(
            &profile_id,
            contact_id,
            now_unix_ms,
        )?;
        let updated_rows = self
            .connection
            .execute(
                "
                UPDATE contacts
                SET friend_relay_consent = ?2,
                    updated_at_unix_ms = ?3
                WHERE contact_id = ?1
                  AND profile_id = ?4
                ",
                params![
                    contact_id.0,
                    bool_to_sqlite_integer(consent),
                    now_unix_ms,
                    profile_id.0,
                ],
            )
            .map_err(map_sqlite_error)?;

        if updated_rows == 0 {
            return Err(OmniError::Storage(format!(
                "contact '{}' was not found",
                contact_id.0
            )));
        }

        self.get_contact(&contact_id)
    }

    /// Friend-proxy (Axis 2): the remote account ids of every APPROVED contact we consented to
    /// carry. Mesh start seeds the in-memory consent set (and derives the advertised
    /// `ANNOUNCE_CAP_FRIEND_RELAY` role — non-empty → advertise) from this.
    pub fn list_friend_relay_consented_account_ids(&self) -> OmniResult<Vec<AccountId>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT DISTINCT remote_account_id
                FROM contacts
                WHERE profile_id = ?1
                  AND removed_at_unix_ms IS NULL
                  AND relationship_state = ?2
                  AND friend_relay_consent = 1
                  AND remote_account_id IS NOT NULL
                ORDER BY remote_account_id ASC
                ",
            )
            .map_err(map_sqlite_error)?;

        let rows = statement
            .query_map(
                params![
                    profile_id.0,
                    omni_identity::ContactRelationshipState::Approved.as_db_value(),
                ],
                |row| row.get::<_, String>(0).map(AccountId),
            )
            .map_err(map_sqlite_error)?;

        let mut account_ids = Vec::new();
        for row in rows {
            account_ids.push(row.map_err(map_sqlite_error)?);
        }
        Ok(account_ids)
    }

    pub fn mark_contact_met(
        &self,
        contact_id: &ContactId,
        now_unix_ms: i64,
    ) -> OmniResult<ContactSnapshot> {
        let profile_id = self.require_active_profile_id()?;
        let contact_id = self.resolve_canonical_contact_id_for_profile(
            &profile_id,
            contact_id,
            now_unix_ms,
        )?;
        let updated_rows = self
            .connection
            .execute(
                "
                UPDATE contacts
                SET has_met = 1,
                    updated_at_unix_ms = ?2
                WHERE contact_id = ?1
                  AND profile_id = ?3
                ",
                params![contact_id.0, now_unix_ms, profile_id.0],
            )
            .map_err(map_sqlite_error)?;

        if updated_rows == 0 {
            return Err(OmniError::Storage(format!(
                "contact '{}' was not found",
                contact_id.0
            )));
        }

        self.get_contact(&contact_id)
    }

    pub fn set_contact_blocked(
        &self,
        contact_id: &ContactId,
        is_blocked: bool,
        now_unix_ms: i64,
    ) -> OmniResult<ContactSnapshot> {
        if is_blocked {
            self.decline_contact(contact_id, now_unix_ms)
        } else {
            self.approve_contact(contact_id, now_unix_ms)
        }
    }

    pub fn forget_contact(
        &self,
        contact_id: &ContactId,
        now_unix_ms: i64,
    ) -> OmniResult<ContactSnapshot> {
        let profile_id = self.require_active_profile_id()?;
        let contact_id = self.resolve_canonical_contact_id_for_profile(
            &profile_id,
            contact_id,
            now_unix_ms,
        )?;
        let existing = self.get_contact(&contact_id)?;
        let mut dialog_keys_to_delete = self
            .find_dialog_keys_for_contact_in_profile(&profile_id, &contact_id)?
            .into_iter()
            .filter(|dialog_key| !dialog_key.is_empty())
            .collect::<Vec<_>>();
        if let Some(primary_dialog_key) = existing
            .primary_dialog_key
            .as_deref()
            .map(omni_types::normalize_dialog_key)
            .filter(|dialog_key| !dialog_key.is_empty())
        {
            if !dialog_keys_to_delete.contains(&primary_dialog_key) {
                dialog_keys_to_delete.push(primary_dialog_key);
            }
        }
        if let Some(instance_dialog_key) = existing
            .remote_instance_id
            .as_ref()
            .map(|instance_id| omni_types::dialog_key_from_instance_id(instance_id))
        {
            if !dialog_keys_to_delete.contains(&instance_dialog_key) {
                dialog_keys_to_delete.push(instance_dialog_key);
            }
        }
        for dialog_key in dialog_keys_to_delete {
            self.delete_dialog_history(&dialog_key)?;
        }
        if let Some(account_id) = existing.remote_account_id.as_ref() {
            // Where its devices post and look is nothing to us any more.
            self.connection
                .execute(
                    "DELETE FROM peer_courier_sets WHERE profile_id = ?1 AND remote_account_id = ?2",
                    params![profile_id.0, account_id.0],
                )
                .map_err(map_sqlite_error)?;
        }
        self.connection
            .execute(
                "DELETE FROM contact_dialog_links WHERE contact_id = ?1 AND profile_id = ?2",
                params![contact_id.0, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "DELETE FROM contact_verifications WHERE contact_id = ?1 AND profile_id = ?2",
                params![contact_id.0, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "DELETE FROM contact_identity_observations WHERE contact_id = ?1 AND profile_id = ?2",
                params![contact_id.0, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "DELETE FROM contact_presence WHERE contact_id = ?1 AND profile_id = ?2",
                params![contact_id.0, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "DELETE FROM message_security_metadata WHERE contact_id = ?1 AND profile_id = ?2",
                params![contact_id.0, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        let updated_rows = self
            .connection
            .execute(
                "
                DELETE FROM contacts
                WHERE contact_id = ?1
                  AND profile_id = ?2
                ",
                params![contact_id.0, profile_id.0],
            )
            .map_err(map_sqlite_error)?;

        if updated_rows == 0 {
            return Err(OmniError::Storage(format!(
                "contact '{}' was not found",
                contact_id.0
            )));
        }

        Ok(ContactSnapshot {
            removed_at_unix_ms: Some(now_unix_ms),
            updated_at_unix_ms: now_unix_ms,
            ..existing
        })
    }

    pub fn list_contacts(&self) -> OmniResult<Vec<ContactSnapshot>> {
        // Pure read: no dedup write on the hot path. Every mutation site that could plant a
        // duplicate (`find_contact_by_remote_account_id`, `find_contact_by_remote_instance_id`,
        // `resolve_contact_for_dialog`) already runs `merge_candidate_contacts_for_profile`
        // BEFORE inserting, so the list can only see already-merged rows. Doing the merge here
        // as well only bought us a write-lock inside a read — the UI thread would block up to
        // 5 s on the WAL busy timeout while background workers wrote, then throw SQLITE_BUSY
        // (live-observed 2026-07-20, Contacts tab freeze/crash after forget). Explicit
        // background dedup runs on writes; nothing else needs it here.
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT
                    c.contact_id,
                    c.petname,
                    c.public_display_name,
                    c.public_avatar_hash_hex,
                    c.public_avatar_mime_type,
                    c.public_avatar_pending_hash_hex,
                    c.public_avatar_pending_mime_type,
                    c.remote_instance_id,
                    (
                        SELECT dialog_key
                        FROM contact_dialog_links
                        WHERE contact_id = c.contact_id
                          AND profile_id = c.profile_id
                          AND is_primary = 1
                        ORDER BY updated_at_unix_ms DESC
                        LIMIT 1
                    ) AS primary_dialog_key,
                    c.remote_account_id,
                    c.public_profile_updated_at_unix_ms,
                    c.relationship_state,
                    c.pending_request_direction,
                    c.has_met,
                    c.friend_relay_consent,
                    COALESCE(p.presence_state, 'offline') AS presence_state,
                    p.last_seen_at_unix_ms,
                    p.last_sent_at_unix_ms,
                    p.last_activity_at_unix_ms,
                    c.removed_at_unix_ms,
                    c.created_at_unix_ms,
                    c.updated_at_unix_ms
                FROM contacts c
                LEFT JOIN contact_presence p
                  ON p.contact_id = c.contact_id
                 AND p.profile_id = c.profile_id
                WHERE c.profile_id = ?1
                  AND c.removed_at_unix_ms IS NULL
                ORDER BY c.petname COLLATE NOCASE ASC, c.contact_id ASC
                ",
            )
            .map_err(map_sqlite_error)?;

        let rows = statement
            .query_map(params![profile_id.0], map_contact_row)
            .map_err(map_sqlite_error)?;

        let mut contacts = Vec::new();
        for row in rows {
            let contact = row.map_err(map_sqlite_error)?;
            contacts.push(self.hydrate_contact_public_avatar_image_path(&profile_id, contact));
        }

        Ok(contacts)
    }

    /// The approved contacts the mesh keeps a live link with — the active circle
    /// (docs/bugs/autoconnect-holds-a-link-to-every-contact.md, maintainers' choice 2026-10-14): added,
    /// heard from or written with since `since_unix_ms`, or with something queued for them. Everyone
    /// else is reached by durable delivery and the couriers, and joins the circle the moment there is
    /// something to send. One query; each check rides an index.
    pub fn list_recently_active_contact_accounts(&self, since_unix_ms: i64) -> OmniResult<Vec<AccountId>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT DISTINCT c.remote_account_id
                FROM contacts c
                LEFT JOIN contact_presence p
                  ON p.contact_id = c.contact_id
                 AND p.profile_id = c.profile_id
                WHERE c.profile_id = ?1
                  AND c.removed_at_unix_ms IS NULL
                  AND c.remote_account_id IS NOT NULL
                  AND c.relationship_state = ?3
                  AND (
                      c.created_at_unix_ms >= ?2
                      OR COALESCE(p.last_activity_at_unix_ms, 0) >= ?2
                      OR EXISTS (
                          SELECT 1 FROM contact_dialog_links l
                          JOIN messages m ON m.profile_id = l.profile_id AND m.dialog_key = l.dialog_key
                          WHERE l.contact_id = c.contact_id
                            AND l.profile_id = c.profile_id
                            AND m.created_at_unix_ms >= ?2
                      )
                      OR EXISTS (
                          SELECT 1 FROM contact_dialog_links l
                          JOIN outbound_queue q ON q.profile_id = l.profile_id AND q.dialog_key = l.dialog_key
                          WHERE l.contact_id = c.contact_id
                            AND l.profile_id = c.profile_id
                      )
                  )
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(
                params![
                    profile_id.0,
                    since_unix_ms,
                    omni_identity::ContactRelationshipState::Approved.as_db_value()
                ],
                |row| Ok(AccountId(row.get::<_, String>(0)?)),
            )
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<_, _>>().map_err(map_sqlite_error)
    }

    /// When each contact account was last heard (newest across its presence rows), from storage —
    /// it outlives a restart, unlike the rendezvous worker's own memory. One query.
    pub fn contact_last_activity_by_account(
        &self,
    ) -> OmniResult<std::collections::HashMap<AccountId, i64>> {
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT c.remote_account_id, MAX(p.last_activity_at_unix_ms)
                FROM contact_presence p
                JOIN contacts c
                  ON c.contact_id = p.contact_id
                 AND c.profile_id = p.profile_id
                WHERE p.profile_id = ?1
                  AND c.remote_account_id IS NOT NULL
                  AND p.last_activity_at_unix_ms IS NOT NULL
                GROUP BY c.remote_account_id
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![profile_id.0], |row| {
                Ok((AccountId(row.get::<_, String>(0)?), row.get::<_, i64>(1)?))
            })
            .map_err(map_sqlite_error)?;
        rows.collect::<Result<_, _>>().map_err(map_sqlite_error)
    }

    pub fn list_contacts_due_for_presence_transition(
        &self,
        max_last_activity_at_unix_ms: i64,
    ) -> OmniResult<Vec<ContactSnapshot>> {
        // Pure read (see `list_contacts` above for the rationale). This one is called from the
        // presence tick on the queue worker — a write here would fight the same worker's own
        // presence-write path for the connection.
        let profile_id = self.require_active_profile_id()?;
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT
                    c.contact_id,
                    c.petname,
                    c.public_display_name,
                    c.public_avatar_hash_hex,
                    c.public_avatar_mime_type,
                    c.public_avatar_pending_hash_hex,
                    c.public_avatar_pending_mime_type,
                    c.remote_instance_id,
                    (
                        SELECT dialog_key
                        FROM contact_dialog_links
                        WHERE contact_id = c.contact_id
                          AND profile_id = c.profile_id
                          AND is_primary = 1
                        ORDER BY updated_at_unix_ms DESC
                        LIMIT 1
                    ) AS primary_dialog_key,
                    c.remote_account_id,
                    c.public_profile_updated_at_unix_ms,
                    c.relationship_state,
                    c.pending_request_direction,
                    c.has_met,
                    c.friend_relay_consent,
                    COALESCE(p.presence_state, 'offline') AS presence_state,
                    p.last_seen_at_unix_ms,
                    p.last_sent_at_unix_ms,
                    p.last_activity_at_unix_ms,
                    c.removed_at_unix_ms,
                    c.created_at_unix_ms,
                    c.updated_at_unix_ms
                FROM contact_presence p
                JOIN contacts c
                  ON c.contact_id = p.contact_id
                 AND c.profile_id = p.profile_id
                WHERE p.profile_id = ?1
                  AND p.presence_state <> 'offline'
                  AND COALESCE(p.last_activity_at_unix_ms, 0) <= ?3
                  AND c.removed_at_unix_ms IS NULL
                  AND c.relationship_state = ?2
                ORDER BY c.petname COLLATE NOCASE ASC, c.contact_id ASC
                ",
            )
            .map_err(map_sqlite_error)?;

        let rows = statement
            .query_map(
                params![
                    profile_id.0,
                    omni_identity::ContactRelationshipState::Approved.as_db_value(),
                    max_last_activity_at_unix_ms,
                ],
                map_contact_row,
            )
            .map_err(map_sqlite_error)?;

        let mut contacts = Vec::new();
        for row in rows {
            let contact = row.map_err(map_sqlite_error)?;
            contacts.push(self.hydrate_contact_public_avatar_image_path(&profile_id, contact));
        }

        Ok(contacts)
    }

    pub fn get_contacts_refresh_summary(&self) -> OmniResult<omni_types::ContactsRefreshSummary> {
        let profile_id = self.require_active_profile_id()?;
        let contact_count = self
            .connection
            .query_row(
                "
                SELECT COUNT(*)
                FROM contacts
                WHERE profile_id = ?1
                  AND removed_at_unix_ms IS NULL
                ",
                params![profile_id.0],
                |row| row.get::<_, i64>(0),
            )
            .map_err(map_sqlite_error)? as usize;
        let last_contact_updated_at_unix_ms = self
            .connection
            .query_row(
                "
                SELECT MAX(updated_at_unix_ms)
                FROM contacts
                WHERE profile_id = ?1
                  AND removed_at_unix_ms IS NULL
                ",
                params![profile_id.0],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(map_sqlite_error)?;
        let last_public_profile_updated_at_unix_ms = self
            .connection
            .query_row(
                "
                SELECT MAX(public_profile_updated_at_unix_ms)
                FROM contacts
                WHERE profile_id = ?1
                  AND removed_at_unix_ms IS NULL
                ",
                params![profile_id.0],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(map_sqlite_error)?;
        let last_presence_updated_at_unix_ms = self
            .connection
            .query_row(
                "
                SELECT MAX(updated_at_unix_ms)
                FROM contact_presence
                WHERE profile_id = ?1
                ",
                params![profile_id.0],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(map_sqlite_error)?;
        let last_verification_updated_at_unix_ms = self
            .connection
            .query_row(
                "
                SELECT MAX(updated_at_unix_ms)
                FROM contact_verifications
                WHERE profile_id = ?1
                ",
                params![profile_id.0],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(map_sqlite_error)?;
        let last_identity_observation_updated_at_unix_ms = self
            .connection
            .query_row(
                "
                SELECT MAX(updated_at_unix_ms)
                FROM contact_identity_observations
                WHERE profile_id = ?1
                ",
                params![profile_id.0],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(map_sqlite_error)?;
        let last_contact_devices_updated_at_unix_ms = self.last_contact_devices_updated_at(None)?;

        Ok(omni_types::ContactsRefreshSummary {
            contact_count,
            last_contact_updated_at_unix_ms,
            last_public_profile_updated_at_unix_ms,
            last_presence_updated_at_unix_ms,
            last_verification_updated_at_unix_ms,
            last_identity_observation_updated_at_unix_ms,
            last_contact_devices_updated_at_unix_ms,
        })
    }

    /// When the devices we know of a contact (of every contact, `None`) last changed: a bundle came
    /// or went, or a device was taken back — what the card's device count follows.
    fn last_contact_devices_updated_at(&self, account_id: Option<&AccountId>) -> OmniResult<Option<i64>> {
        let filter = account_id.map(|account_id| account_id.0.as_str());
        self.connection
            .query_row(
                "
                SELECT MAX(at) FROM (
                    SELECT MAX(updated_at_unix_ms) AS at FROM remote_prekey_bundles
                    WHERE ?1 IS NULL OR remote_account_id = ?1
                    UNION ALL
                    SELECT MAX(revoked_at_unix_ms) AS at FROM device_revocations
                    WHERE ?1 IS NULL OR account_id = ?1
                )
                ",
                params![filter],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(map_sqlite_error)
    }

    pub fn get_contact_card(&self, contact_id: &ContactId) -> OmniResult<ContactCardSnapshot> {
        let contact = self.get_contact(contact_id)?;
        let observation = self.get_effective_contact_observation(&contact)?;
        let verification = self.get_contact_verification(&contact.contact_id)?;
        let (trust_state, verification_summary) =
            evaluate_trust(observation.as_ref(), verification.as_ref(), self.contact_device_count(&contact));

        Ok(ContactCardSnapshot {
            contact,
            trust_state,
            verification: verification_summary,
        })
    }

    pub fn get_contact_card_revision(
        &self,
        contact_id: &ContactId,
    ) -> OmniResult<Option<omni_types::EntityRevisionSnapshot>> {
        let profile_id = self.require_active_profile_id()?;
        let resolved_contact_id =
            self.resolve_canonical_contact_id_for_profile(&profile_id, contact_id, current_unix_ms())?;
        let contact = self.connection
            .query_row(
                "
                SELECT updated_at_unix_ms, COALESCE(public_profile_updated_at_unix_ms, 0)
                FROM contacts
                WHERE contact_id = ?1
                  AND profile_id = ?2
                ",
                params![resolved_contact_id.0, profile_id.0],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let Some((contact_updated_at_unix_ms, public_profile_updated_at_unix_ms)) = contact else {
            return Ok(None);
        };
        let presence_updated_at_unix_ms = self
            .connection
            .query_row(
                "
                SELECT COALESCE(MAX(updated_at_unix_ms), 0)
                FROM contact_presence
                WHERE contact_id = ?1
                  AND profile_id = ?2
                ",
                params![resolved_contact_id.0, profile_id.0],
                |row| row.get::<_, i64>(0),
            )
            .map_err(map_sqlite_error)?;
        let verification_updated_at_unix_ms = self
            .connection
            .query_row(
                "
                SELECT COALESCE(MAX(updated_at_unix_ms), 0)
                FROM contact_verifications
                WHERE contact_id = ?1
                  AND profile_id = ?2
                ",
                params![resolved_contact_id.0, profile_id.0],
                |row| row.get::<_, i64>(0),
            )
            .map_err(map_sqlite_error)?;
        let identity_observation_updated_at_unix_ms = self
            .connection
            .query_row(
                "
                SELECT COALESCE(MAX(updated_at_unix_ms), 0)
                FROM contact_identity_observations
                WHERE contact_id = ?1
                  AND profile_id = ?2
                ",
                params![resolved_contact_id.0, profile_id.0],
                |row| row.get::<_, i64>(0),
            )
            .map_err(map_sqlite_error)?;
        let remote_account_id = self
            .connection
            .query_row(
                "SELECT remote_account_id FROM contacts WHERE contact_id = ?1 AND profile_id = ?2",
                params![resolved_contact_id.0, profile_id.0],
                |row| row.get::<_, Option<String>>(0),
            )
            .map_err(map_sqlite_error)?;
        let devices_updated_at_unix_ms = match remote_account_id {
            Some(account_id) => self.last_contact_devices_updated_at(Some(&AccountId(account_id)))?.unwrap_or(0),
            None => 0,
        };

        Ok(Some(omni_types::EntityRevisionSnapshot {
            entity_id: resolved_contact_id.0,
            revision: [
                contact_updated_at_unix_ms,
                public_profile_updated_at_unix_ms,
                presence_updated_at_unix_ms,
                verification_updated_at_unix_ms,
                identity_observation_updated_at_unix_ms,
                devices_updated_at_unix_ms,
            ]
            .into_iter()
            .max()
            .unwrap_or(0),
        }))
    }

    /// The chat a dialog belongs to: a device dialog of a contact maps to the contact's primary
    /// dialog, anything else to itself.
    pub fn chat_dialog_key_for(&self, dialog_key: &str) -> OmniResult<String> {
        let dialog_key = omni_types::normalize_dialog_key(dialog_key);
        Ok(self
            .resolve_contact_for_dialog(&dialog_key)?
            .and_then(|contact| contact.primary_dialog_key)
            .map(|primary| omni_types::normalize_dialog_key(&primary))
            .unwrap_or(dialog_key))
    }

    pub fn resolve_contact_for_dialog(&self, dialog_key: &str) -> OmniResult<Option<ContactSnapshot>> {
        let profile_id = self.require_active_profile_id()?;
        let candidates = self.find_contacts_for_dialog_in_profile(&profile_id, dialog_key)?;
        self.merge_candidate_contacts_for_profile(&profile_id, candidates, current_unix_ms())
    }

    pub fn find_contact_by_remote_account_id(
        &self,
        remote_account_id: &AccountId,
    ) -> OmniResult<Option<ContactSnapshot>> {
        let profile_id = self.require_active_profile_id()?;
        let candidates =
            self.find_contacts_by_remote_account_id_in_profile(&profile_id, remote_account_id)?;
        self.merge_candidate_contacts_for_profile(&profile_id, candidates, current_unix_ms())
    }

    pub fn has_removed_contact_for_remote_account(
        &self,
        remote_account_id: &AccountId,
    ) -> OmniResult<bool> {
        let profile_id = self.require_active_profile_id()?;
        let count = self
            .connection
            .query_row(
                "
                SELECT COUNT(*)
                FROM contacts
                WHERE profile_id = ?2
                  AND remote_account_id = ?1
                  AND removed_at_unix_ms IS NOT NULL
                ",
                params![remote_account_id.0, profile_id.0],
                |row| row.get::<_, i64>(0),
            )
            .map_err(map_sqlite_error)?;
        Ok(count > 0)
    }

    pub fn find_contact_by_remote_instance_id(
        &self,
        remote_instance_id: &str,
    ) -> OmniResult<Option<ContactSnapshot>> {
        let profile_id = self.require_active_profile_id()?;
        let candidates =
            self.find_contacts_by_remote_instance_id_in_profile(&profile_id, remote_instance_id)?;
        self.merge_candidate_contacts_for_profile(&profile_id, candidates, current_unix_ms())
    }

    pub fn resolve_contact_card_for_dialog(
        &self,
        dialog_key: &str,
    ) -> OmniResult<Option<ContactCardSnapshot>> {
        match self.resolve_contact_for_dialog(dialog_key)? {
            Some(contact) => self.get_contact_card(&contact.contact_id).map(Some),
            None => Ok(None),
        }
    }

    pub fn get_message_security_details(
        &self,
        message_id: &str,
    ) -> OmniResult<Option<MessageSecurityDetails>> {
        let profile_id = self.require_active_profile_id()?;
        let metadata = self.connection
            .query_row(
                "
                SELECT
                    message_id,
                    dialog_key,
                    contact_id,
                    session_address,
                    remote_account_id,
                    remote_device_id,
                    encryption_state,
                    bootstrap_state,
                    payload_kind
                FROM message_security_metadata
                WHERE message_id = ?1
                  AND profile_id = ?2
                ",
                params![message_id, profile_id.0],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, Option<String>>(8)?,
                    ))
                },
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let Some((
            message_id,
            dialog_key,
            contact_id,
            session_address,
            remote_account_id,
            remote_device_id,
            encryption_state,
            bootstrap_state,
            payload_kind,
        )) = metadata
        else {
            return Ok(None);
        };
        let trust_state = contact_id.as_ref().and_then(|value| {
            self.get_contact_card(&ContactId(value.clone()))
                .ok()
                .map(|card| card.trust_state)
        });
        let remote_device_label = match (&remote_account_id, &remote_device_id) {
            (Some(account_id), Some(device_id)) => self
                .get_remote_device_label(&AccountId(account_id.clone()), &DeviceId(device_id.clone()))?,
            _ => None,
        };
        Ok(Some(MessageSecurityDetails {
            message_id,
            dialog_key,
            contact_id: contact_id.map(ContactId),
            session_address,
            remote_account_id,
            remote_device_id,
            remote_device_label,
            trust_state,
            encryption_state: MessageEncryptionState::from_db_value(&encryption_state)?,
            bootstrap_state: bootstrap_state
                .map(|value| SessionBootstrapState::from_db_value(&value))
                .transpose()?,
            payload_kind: payload_kind
                .map(|value| SecurityPayloadKind::from_db_value(&value))
                .transpose()?,
            has_encrypted_payload: false,
        }))
    }

    pub fn get_message_security_details_batch(
        &self,
        message_ids: &[String],
    ) -> OmniResult<Vec<MessageSecurityDetails>> {
        if message_ids.is_empty() {
            return Ok(Vec::new());
        }

        let profile_id = self.require_active_profile_id()?;
        let mut ordered_message_ids = Vec::new();
        let mut seen_message_ids = std::collections::HashSet::new();
        for message_id in message_ids {
            if seen_message_ids.insert(message_id.as_str()) {
                ordered_message_ids.push(message_id.clone());
            }
        }

        let placeholders = (0..ordered_message_ids.len())
            .map(|index| format!("?{}", index + 2))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "
            SELECT
                message_id,
                dialog_key,
                contact_id,
                session_address,
                remote_account_id,
                remote_device_id,
                encryption_state,
                bootstrap_state,
                payload_kind
            FROM message_security_metadata
            WHERE profile_id = ?1
              AND message_id IN ({placeholders})
            "
        );
        let mut bind_values = Vec::with_capacity(ordered_message_ids.len() + 1);
        bind_values.push(rusqlite::types::Value::from(profile_id.0.clone()));
        bind_values.extend(
            ordered_message_ids
                .iter()
                .cloned()
                .map(rusqlite::types::Value::from),
        );

        let mut statement = self.connection.prepare(&sql).map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(bind_values), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                ))
            })
            .map_err(map_sqlite_error)?;

        let mut metadata_by_message_id = std::collections::HashMap::new();
        for row in rows {
            let (
                message_id,
                dialog_key,
                contact_id,
                session_address,
                remote_account_id,
                remote_device_id,
                encryption_state,
                bootstrap_state,
                payload_kind,
            ) = row.map_err(map_sqlite_error)?;
            metadata_by_message_id.insert(
                message_id,
                (
                    dialog_key,
                    contact_id,
                    session_address,
                    remote_account_id,
                    remote_device_id,
                    encryption_state,
                    bootstrap_state,
                    payload_kind,
                ),
            );
        }

        let mut trust_state_by_contact_id = std::collections::HashMap::new();
        let mut remote_device_label_by_key =
            std::collections::HashMap::<String, Option<String>>::new();
        let mut items = Vec::with_capacity(ordered_message_ids.len());
        for message_id in ordered_message_ids {
            let Some((
                dialog_key,
                contact_id,
                session_address,
                remote_account_id,
                remote_device_id,
                encryption_state,
                bootstrap_state,
                payload_kind,
            )) = metadata_by_message_id.remove(&message_id)
            else {
                continue;
            };

            let trust_state = if let Some(contact_id_value) = contact_id.as_ref() {
                if let Some(cached) = trust_state_by_contact_id.get(contact_id_value) {
                    *cached
                } else {
                    let resolved = self
                        .get_contact_card(&ContactId(contact_id_value.clone()))
                        .ok()
                        .map(|card| card.trust_state);
                    trust_state_by_contact_id.insert(contact_id_value.clone(), resolved);
                    resolved
                }
            } else {
                None
            };

            let remote_device_label = match (&remote_account_id, &remote_device_id) {
                (Some(account_id), Some(device_id)) => {
                    let cache_key = format!("{account_id}:{device_id}");
                    if let Some(cached) = remote_device_label_by_key.get(&cache_key) {
                        cached.clone()
                    } else {
                        let resolved = self.get_remote_device_label(
                            &AccountId(account_id.clone()),
                            &DeviceId(device_id.clone()),
                        )?;
                        remote_device_label_by_key.insert(cache_key, resolved.clone());
                        resolved
                    }
                }
                _ => None,
            };

            items.push(MessageSecurityDetails {
                message_id,
                dialog_key,
                contact_id: contact_id.map(ContactId),
                session_address,
                remote_account_id,
                remote_device_id,
                remote_device_label,
                trust_state,
                encryption_state: MessageEncryptionState::from_db_value(&encryption_state)?,
                bootstrap_state: bootstrap_state
                    .map(|value| SessionBootstrapState::from_db_value(&value))
                    .transpose()?,
                payload_kind: payload_kind
                    .map(|value| SecurityPayloadKind::from_db_value(&value))
                    .transpose()?,
                has_encrypted_payload: false,
            });
        }

        Ok(items)
    }

    pub fn get_message_security_details_delta(
        &self,
        dialog_key: &str,
        after_updated_at_unix_ms: Option<i64>,
        after_message_id: Option<&str>,
    ) -> OmniResult<Vec<MessageSecurityDetails>> {
        let Some(after_updated_at_unix_ms) = after_updated_at_unix_ms else {
            return Ok(Vec::new());
        };

        let profile_id = self.require_active_profile_id()?;
        let normalized_dialog_key = omni_types::normalize_dialog_key(dialog_key);
        let after_message_id = after_message_id.unwrap_or_default();
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT message_id
                FROM message_security_metadata
                WHERE dialog_key = ?1
                  AND profile_id = ?2
                  AND (
                      updated_at_unix_ms > ?3 OR
                      (updated_at_unix_ms = ?3 AND message_id > ?4)
                  )
                ORDER BY updated_at_unix_ms ASC, message_id ASC
                ",
            )
            .map_err(map_sqlite_error)?;

        let rows = statement
            .query_map(
                params![
                    normalized_dialog_key.as_str(),
                    profile_id.0.as_str(),
                    after_updated_at_unix_ms,
                    after_message_id,
                ],
                |row| row.get::<_, String>(0),
            )
            .map_err(map_sqlite_error)?;

        let mut message_ids = Vec::new();
        for row in rows {
            message_ids.push(row.map_err(map_sqlite_error)?);
        }

        self.get_message_security_details_batch(&message_ids)
    }

    pub fn link_dialog_to_contact(
        &self,
        contact_id: &ContactId,
        dialog_key: &str,
        is_primary: bool,
        now_unix_ms: i64,
    ) -> OmniResult<ContactSnapshot> {
        let profile_id = self.require_active_profile_id()?;
        let contact_id = self.resolve_canonical_contact_id_for_profile(
            &profile_id,
            contact_id,
            now_unix_ms,
        )?;
        let _ = self.get_contact(&contact_id)?;
        self.upsert_contact_dialog_link(&contact_id, dialog_key, is_primary, now_unix_ms)?;
        self.get_contact(&contact_id)
    }

    pub fn bind_contact_to_remote_instance(
        &self,
        contact_id: &ContactId,
        remote_instance_id: &str,
        primary_dialog_key: Option<&str>,
        now_unix_ms: i64,
    ) -> OmniResult<ContactSnapshot> {
        let profile_id = self.require_active_profile_id()?;
        let canonical_contact_id = self.resolve_canonical_contact_id_for_profile(
            &profile_id,
            contact_id,
            now_unix_ms,
        )?;
        let remote_instance_id = remote_instance_id.trim();
        if remote_instance_id.is_empty() {
            return Err(OmniError::InvalidInput(
                "remote_instance_id must not be empty".to_string(),
            ));
        }

        let updated_rows = self
            .connection
            .execute(
                "
                UPDATE contacts
                SET remote_instance_id = ?2,
                    updated_at_unix_ms = ?3
                WHERE contact_id = ?1
                  AND profile_id = ?4
                ",
                params![
                    canonical_contact_id.0,
                    remote_instance_id,
                    now_unix_ms,
                    profile_id.0,
                ],
            )
            .map_err(map_sqlite_error)?;
        if updated_rows == 0 {
            return Err(OmniError::Storage(format!(
                "contact with id '{}' was not found",
                canonical_contact_id.0
            )));
        }

        if let Some(dialog_key) = primary_dialog_key {
            let dialog_key = dialog_key.trim();
            if !dialog_key.is_empty() {
                self.upsert_contact_dialog_link(
                    &canonical_contact_id,
                    dialog_key,
                    true,
                    now_unix_ms,
                )?;
            }
        }

        self.get_contact(&canonical_contact_id)
    }

    pub fn verify_contact(
        &self,
        contact_id: &ContactId,
        method: VerificationMethod,
        verified_at_unix_ms: i64,
    ) -> OmniResult<ContactCardSnapshot> {
        let profile_id = self.require_active_profile_id()?;
        let canonical_contact_id = self.resolve_canonical_contact_id_for_profile(
            &profile_id,
            contact_id,
            verified_at_unix_ms,
        )?;
        let contact = self.get_contact(&canonical_contact_id)?;
        if contact.relationship_state != omni_identity::ContactRelationshipState::Approved {
            return Err(OmniError::InvalidInput(
                "cannot verify a contact that is not approved".to_string(),
            ));
        }
        let observation = self.get_effective_contact_observation(&contact)?.ok_or_else(|| {
            OmniError::InvalidInput(
                "cannot verify contact without an identity observation".to_string(),
            )
        })?;
        let baseline = build_verification_baseline(&observation, method, verified_at_unix_ms)?;

        self.connection
            .execute(
                "
                INSERT INTO contact_verifications (
                    contact_id,
                    verified_at_unix_ms,
                    verification_method,
                    verified_identity_key,
                    updated_at_unix_ms,
                    profile_id
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ON CONFLICT(contact_id) DO UPDATE SET
                    verified_at_unix_ms = excluded.verified_at_unix_ms,
                    verification_method = excluded.verification_method,
                    verified_identity_key = excluded.verified_identity_key,
                    updated_at_unix_ms = excluded.updated_at_unix_ms
                ",
                params![
                    canonical_contact_id.0,
                    baseline.verified_at_unix_ms,
                    verification_method_to_db(method),
                    baseline.verified_identity_key,
                    verified_at_unix_ms,
                    profile_id.0,
                ],
            )
            .map_err(map_sqlite_error)?;

        self.get_contact_card(&canonical_contact_id)
    }

    pub fn record_contact_observation(
        &self,
        contact_id: &ContactId,
        observation: &ContactObservationSnapshot,
    ) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        let contact_id = self.resolve_canonical_contact_id_for_profile(
            &profile_id,
            contact_id,
            observation.observed_at_unix_ms,
        )?;
        let _ = self.get_contact(&contact_id)?;
        validate_contact_observation(observation)?;

        if let Some(identity_key) = observation.identity_key.as_deref() {
            self.connection
                .execute(
                    "
                    INSERT INTO contact_identity_observations (
                        contact_id,
                        identity_key,
                        observed_display_name,
                        observed_at_unix_ms,
                        updated_at_unix_ms,
                        profile_id
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                    ON CONFLICT(contact_id) DO UPDATE SET
                        identity_key = excluded.identity_key,
                        observed_display_name = excluded.observed_display_name,
                        observed_at_unix_ms = excluded.observed_at_unix_ms,
                        updated_at_unix_ms = excluded.updated_at_unix_ms
                    ",
                    params![
                        contact_id.0,
                        identity_key,
                        observation.observed_display_name,
                        observation.observed_at_unix_ms,
                        observation.observed_at_unix_ms,
                        profile_id.0,
                    ],
                )
                .map_err(map_sqlite_error)?;
        } else {
            self.connection
                .execute(
                    "DELETE FROM contact_identity_observations WHERE contact_id = ?1 AND profile_id = ?2",
                    params![contact_id.0, profile_id.0],
                )
                .map_err(map_sqlite_error)?;
        }

        Ok(())
    }

    pub fn record_contact_presence_seen(
        &self,
        contact_id: &ContactId,
        observed_at_unix_ms: i64,
    ) -> OmniResult<()> {
        self.upsert_contact_presence(
            contact_id,
            Some(observed_at_unix_ms),
            None,
            Some(observed_at_unix_ms),
            Some(omni_identity::ContactPresenceState::Online),
            Some(0),
            Some(observed_at_unix_ms + next_contact_heartbeat_interval_ms()),
            observed_at_unix_ms,
        )
    }

    pub fn record_contact_presence_sent(
        &self,
        contact_id: &ContactId,
        sent_at_unix_ms: i64,
    ) -> OmniResult<()> {
        self.upsert_contact_presence(
            contact_id,
            None,
            Some(sent_at_unix_ms),
            None,
            None,
            None,
            Some(sent_at_unix_ms + next_contact_heartbeat_interval_ms()),
            sent_at_unix_ms,
        )
    }

    pub fn record_contact_presence_activity(
        &self,
        contact_id: &ContactId,
        activity_at_unix_ms: i64,
    ) -> OmniResult<()> {
        self.upsert_contact_presence(
            contact_id,
            None,
            None,
            Some(activity_at_unix_ms),
            Some(omni_identity::ContactPresenceState::Online),
            Some(0),
            Some(activity_at_unix_ms + next_contact_heartbeat_interval_ms()),
            activity_at_unix_ms,
        )
    }

    pub fn record_contact_presence_pong(
        &self,
        contact_id: &ContactId,
        observed_at_unix_ms: i64,
    ) -> OmniResult<()> {
        self.upsert_contact_presence(
            contact_id,
            Some(observed_at_unix_ms),
            None,
            Some(observed_at_unix_ms),
            Some(omni_identity::ContactPresenceState::Online),
            Some(0),
            Some(observed_at_unix_ms + next_contact_heartbeat_interval_ms()),
            observed_at_unix_ms,
        )
    }

    pub fn record_contact_presence_miss(
        &self,
        contact_id: &ContactId,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        let contact = self.get_contact(contact_id)?;
        let next_miss_count = current_contact_presence_miss_count(self, contact_id)? + 1;
        let state = if next_miss_count >= 2 {
            omni_identity::ContactPresenceState::Recently
        } else {
            contact.presence_state
        };
        self.upsert_contact_presence(
            contact_id,
            None,
            Some(now_unix_ms),
            None,
            Some(state),
            Some(next_miss_count),
            Some(now_unix_ms + next_contact_heartbeat_interval_ms()),
            now_unix_ms,
        )
    }

    pub fn record_contact_presence_recently(
        &self,
        contact_id: &ContactId,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        let miss_count = current_contact_presence_miss_count(self, contact_id)?.max(2);
        self.upsert_contact_presence(
            contact_id,
            None,
            None,
            None,
            Some(omni_identity::ContactPresenceState::Recently),
            Some(miss_count),
            None,
            now_unix_ms,
        )
    }

    pub fn record_contact_presence_offline(
        &self,
        contact_id: &ContactId,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        self.upsert_contact_presence(
            contact_id,
            None,
            None,
            None,
            Some(omni_identity::ContactPresenceState::Offline),
            Some(0),
            None,
            now_unix_ms,
        )
    }

    pub fn next_contact_heartbeat_due_at(
        &self,
        contact_id: &ContactId,
    ) -> OmniResult<Option<i64>> {
        let profile_id = self.require_active_profile_id()?;
        let contact_id = self.resolve_canonical_contact_id_for_profile(
            &profile_id,
            contact_id,
            current_unix_ms(),
        )?;
        let _ = self.get_contact(&contact_id)?;
        self.connection
            .query_row(
                "
                SELECT next_heartbeat_due_at_unix_ms
                FROM contact_presence
                WHERE contact_id = ?1
                  AND profile_id = ?2
                ",
                params![contact_id.0, profile_id.0],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()
            .map_err(map_sqlite_error)
            .map(|value| value.flatten())
    }

    #[allow(clippy::too_many_arguments)] // constructor/builder — args are the record's fields
    fn upsert_contact_presence(
        &self,
        contact_id: &ContactId,
        last_seen_at_unix_ms: Option<i64>,
        last_sent_at_unix_ms: Option<i64>,
        last_activity_at_unix_ms: Option<i64>,
        presence_state: Option<omni_identity::ContactPresenceState>,
        consecutive_missed_heartbeats: Option<i64>,
        next_heartbeat_due_at_unix_ms: Option<i64>,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        let contact_id =
            self.resolve_canonical_contact_id_for_profile(&profile_id, contact_id, now_unix_ms)?;
        let _ = self.get_contact(&contact_id)?;
        self.connection
            .execute(
                "
                INSERT INTO contact_presence (
                    contact_id,
                    profile_id,
                    last_seen_at_unix_ms,
                    last_sent_at_unix_ms,
                    last_activity_at_unix_ms,
                    presence_state,
                    consecutive_missed_heartbeats,
                    next_heartbeat_due_at_unix_ms,
                    created_at_unix_ms,
                    updated_at_unix_ms
                )
                VALUES (?1, ?2, ?3, ?4, ?5, COALESCE(?6, 'offline'), COALESCE(?7, 0), ?8, ?9, ?9)
                ON CONFLICT(contact_id, profile_id) DO UPDATE SET
                    last_seen_at_unix_ms = COALESCE(excluded.last_seen_at_unix_ms, contact_presence.last_seen_at_unix_ms),
                    last_sent_at_unix_ms = COALESCE(excluded.last_sent_at_unix_ms, contact_presence.last_sent_at_unix_ms),
                    last_activity_at_unix_ms = COALESCE(excluded.last_activity_at_unix_ms, contact_presence.last_activity_at_unix_ms),
                    presence_state = COALESCE(excluded.presence_state, contact_presence.presence_state),
                    consecutive_missed_heartbeats = COALESCE(excluded.consecutive_missed_heartbeats, contact_presence.consecutive_missed_heartbeats),
                    next_heartbeat_due_at_unix_ms = excluded.next_heartbeat_due_at_unix_ms,
                    updated_at_unix_ms = excluded.updated_at_unix_ms
                ",
                params![
                    contact_id.0,
                    profile_id.0,
                    last_seen_at_unix_ms,
                    last_sent_at_unix_ms,
                    last_activity_at_unix_ms,
                    presence_state.map(|value| value.as_db_value()),
                    consecutive_missed_heartbeats,
                    next_heartbeat_due_at_unix_ms,
                    now_unix_ms,
                ],
            )
            .map_err(map_sqlite_error)?;

        Ok(())
    }

    /// Full-sweep duplicate merge for the active profile. This used to run inside every
    /// `list_contacts`, which put a write on the UI's read path — see the comment on
    /// `list_contacts`. Now called from the autoconnect worker's periodic tick instead, so the
    /// UI never blocks on it. Write-time `find_*` merges still cover the mutation paths, so a
    /// duplicate can only linger between an insert and the next background sweep.
    pub fn merge_duplicate_contacts_for_active_profile(&self, now_unix_ms: i64) -> OmniResult<()> {
        let profile_id = self.require_active_profile_id()?;
        self.merge_duplicate_contacts_for_profile(&profile_id, now_unix_ms)
    }

    fn merge_duplicate_contacts_for_profile(
        &self,
        profile_id: &LocalProfileId,
        now_unix_ms: i64,
    ) -> OmniResult<()> {
        let mut account_id_statement = self
            .connection
            .prepare(
                "
                SELECT remote_account_id
                FROM contacts
                WHERE profile_id = ?1
                  AND removed_at_unix_ms IS NULL
                  AND remote_account_id IS NOT NULL
                GROUP BY remote_account_id
                HAVING COUNT(*) > 1
                ",
            )
            .map_err(map_sqlite_error)?;
        let account_ids = account_id_statement
            .query_map(params![profile_id.0], |row| row.get::<_, String>(0))
            .map_err(map_sqlite_error)?;
        for account_id in account_ids {
            let account_id = AccountId(account_id.map_err(map_sqlite_error)?);
            let candidates =
                self.find_contacts_by_remote_account_id_in_profile(profile_id, &account_id)?;
            let _ = self.merge_candidate_contacts_for_profile(profile_id, candidates, now_unix_ms)?;
        }

        let mut dialog_statement = self
            .connection
            .prepare(
                "
                SELECT l.dialog_key
                FROM contact_dialog_links l
                JOIN contacts c
                  ON c.contact_id = l.contact_id
                 AND c.profile_id = l.profile_id
                WHERE l.profile_id = ?1
                  AND c.removed_at_unix_ms IS NULL
                GROUP BY l.dialog_key
                HAVING COUNT(DISTINCT l.contact_id) > 1
                ",
            )
            .map_err(map_sqlite_error)?;
        let dialog_keys = dialog_statement
            .query_map(params![profile_id.0], |row| row.get::<_, String>(0))
            .map_err(map_sqlite_error)?;
        for dialog_key in dialog_keys {
            let dialog_key = dialog_key.map_err(map_sqlite_error)?;
            let candidates = self.find_contacts_for_dialog_in_profile(profile_id, &dialog_key)?;
            let _ = self.merge_candidate_contacts_for_profile(profile_id, candidates, now_unix_ms)?;
        }

        Ok(())
    }

    fn resolve_canonical_contact_id_for_profile(
        &self,
        profile_id: &LocalProfileId,
        contact_id: &ContactId,
        now_unix_ms: i64,
    ) -> OmniResult<ContactId> {
        let existing = self
            .get_contact_in_profile(profile_id, contact_id)?
            .ok_or_else(|| OmniError::Storage(format!("contact '{}' was not found", contact_id.0)))?;

        let mut candidates = vec![existing.clone()];
        if let Some(remote_account_id) = existing.remote_account_id.as_ref() {
            candidates.extend(
                self.find_contacts_by_remote_account_id_in_profile(profile_id, remote_account_id)?,
            );
        }
        if let Some(primary_dialog_key) = existing.primary_dialog_key.as_deref() {
            candidates.extend(self.find_contacts_for_dialog_in_profile(
                profile_id,
                primary_dialog_key,
            )?);
        }
        if let Some(remote_instance_id) = existing.remote_instance_id.as_deref() {
            candidates.extend(self.find_contacts_by_remote_instance_id_in_profile(
                profile_id,
                remote_instance_id,
            )?);
        }

        Ok(self
            .merge_candidate_contacts_for_profile(profile_id, candidates, now_unix_ms)?
            .unwrap_or(existing)
            .contact_id)
    }

    fn get_contact_in_profile(
        &self,
        profile_id: &LocalProfileId,
        contact_id: &ContactId,
    ) -> OmniResult<Option<ContactSnapshot>> {
        self.connection
            .query_row(
                "
                SELECT
                    c.contact_id,
                    c.petname,
                    c.public_display_name,
                    c.public_avatar_hash_hex,
                    c.public_avatar_mime_type,
                    c.public_avatar_pending_hash_hex,
                    c.public_avatar_pending_mime_type,
                    c.remote_instance_id,
                    (
                        SELECT dialog_key
                        FROM contact_dialog_links
                        WHERE contact_id = c.contact_id
                          AND profile_id = c.profile_id
                          AND is_primary = 1
                        ORDER BY updated_at_unix_ms DESC
                        LIMIT 1
                    ) AS primary_dialog_key,
                    c.remote_account_id,
                    c.public_profile_updated_at_unix_ms,
                    c.relationship_state,
                    c.pending_request_direction,
                    c.has_met,
                    c.friend_relay_consent,
                    COALESCE(p.presence_state, 'offline') AS presence_state,
                    p.last_seen_at_unix_ms,
                    p.last_sent_at_unix_ms,
                    p.last_activity_at_unix_ms,
                    c.removed_at_unix_ms,
                    c.created_at_unix_ms,
                    c.updated_at_unix_ms
                FROM contacts c
                LEFT JOIN contact_presence p
                  ON p.contact_id = c.contact_id
                 AND p.profile_id = c.profile_id
                WHERE c.contact_id = ?1
                  AND c.profile_id = ?2
                ",
                params![contact_id.0, profile_id.0],
                map_contact_row,
            )
            .optional()
            .map_err(map_sqlite_error)
            .map(|value| {
                value.map(|contact| self.hydrate_contact_public_avatar_image_path(profile_id, contact))
            })
    }

    fn find_contacts_for_dialog_in_profile(
        &self,
        profile_id: &LocalProfileId,
        dialog_key: &str,
    ) -> OmniResult<Vec<ContactSnapshot>> {
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT
                    c.contact_id,
                    c.petname,
                    c.public_display_name,
                    c.public_avatar_hash_hex,
                    c.public_avatar_mime_type,
                    c.public_avatar_pending_hash_hex,
                    c.public_avatar_pending_mime_type,
                    c.remote_instance_id,
                    (
                        SELECT dialog_key
                        FROM contact_dialog_links
                        WHERE contact_id = c.contact_id
                          AND profile_id = c.profile_id
                          AND is_primary = 1
                        ORDER BY updated_at_unix_ms DESC
                        LIMIT 1
                    ) AS primary_dialog_key,
                    c.remote_account_id,
                    c.public_profile_updated_at_unix_ms,
                    c.relationship_state,
                    c.pending_request_direction,
                    c.has_met,
                    c.friend_relay_consent,
                    COALESCE(p.presence_state, 'offline') AS presence_state,
                    p.last_seen_at_unix_ms,
                    p.last_sent_at_unix_ms,
                    p.last_activity_at_unix_ms,
                    c.removed_at_unix_ms,
                    c.created_at_unix_ms,
                    c.updated_at_unix_ms
                FROM contact_dialog_links l
                JOIN contacts c ON c.contact_id = l.contact_id
                LEFT JOIN contact_presence p
                  ON p.contact_id = c.contact_id
                 AND p.profile_id = c.profile_id
                WHERE l.dialog_key = ?1
                  AND l.profile_id = ?2
                  AND c.profile_id = ?2
                  AND c.removed_at_unix_ms IS NULL
                ORDER BY l.is_primary DESC, l.updated_at_unix_ms DESC, c.updated_at_unix_ms DESC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![normalize_dialog_key(dialog_key), profile_id.0], map_contact_row)
            .map_err(map_sqlite_error)?;
        let mut contacts = Vec::new();
        for row in rows {
            let contact = row.map_err(map_sqlite_error)?;
            contacts.push(self.hydrate_contact_public_avatar_image_path(profile_id, contact));
        }
        Ok(contacts)
    }

    fn find_contacts_by_remote_account_id_in_profile(
        &self,
        profile_id: &LocalProfileId,
        remote_account_id: &AccountId,
    ) -> OmniResult<Vec<ContactSnapshot>> {
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT
                    c.contact_id,
                    c.petname,
                    c.public_display_name,
                    c.public_avatar_hash_hex,
                    c.public_avatar_mime_type,
                    c.public_avatar_pending_hash_hex,
                    c.public_avatar_pending_mime_type,
                    c.remote_instance_id,
                    (
                        SELECT dialog_key
                        FROM contact_dialog_links
                        WHERE contact_id = c.contact_id
                          AND profile_id = c.profile_id
                          AND is_primary = 1
                        ORDER BY updated_at_unix_ms DESC
                        LIMIT 1
                    ) AS primary_dialog_key,
                    c.remote_account_id,
                    c.public_profile_updated_at_unix_ms,
                    c.relationship_state,
                    c.pending_request_direction,
                    c.has_met,
                    c.friend_relay_consent,
                    COALESCE(p.presence_state, 'offline') AS presence_state,
                    p.last_seen_at_unix_ms,
                    p.last_sent_at_unix_ms,
                    p.last_activity_at_unix_ms,
                    c.removed_at_unix_ms,
                    c.created_at_unix_ms,
                    c.updated_at_unix_ms
                FROM contacts c
                LEFT JOIN contact_presence p
                  ON p.contact_id = c.contact_id
                 AND p.profile_id = c.profile_id
                WHERE c.remote_account_id = ?1
                  AND c.profile_id = ?2
                  AND c.removed_at_unix_ms IS NULL
                ORDER BY c.updated_at_unix_ms DESC, c.contact_id ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![remote_account_id.0, profile_id.0], map_contact_row)
            .map_err(map_sqlite_error)?;
        let mut contacts = Vec::new();
        for row in rows {
            let contact = row.map_err(map_sqlite_error)?;
            contacts.push(self.hydrate_contact_public_avatar_image_path(profile_id, contact));
        }
        Ok(contacts)
    }

    fn find_contacts_by_remote_instance_id_in_profile(
        &self,
        profile_id: &LocalProfileId,
        remote_instance_id: &str,
    ) -> OmniResult<Vec<ContactSnapshot>> {
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT
                    c.contact_id,
                    c.petname,
                    c.public_display_name,
                    c.public_avatar_hash_hex,
                    c.public_avatar_mime_type,
                    c.public_avatar_pending_hash_hex,
                    c.public_avatar_pending_mime_type,
                    c.remote_instance_id,
                    (
                        SELECT dialog_key
                        FROM contact_dialog_links
                        WHERE contact_id = c.contact_id
                          AND profile_id = c.profile_id
                          AND is_primary = 1
                        ORDER BY updated_at_unix_ms DESC
                        LIMIT 1
                    ) AS primary_dialog_key,
                    c.remote_account_id,
                    c.public_profile_updated_at_unix_ms,
                    c.relationship_state,
                    c.pending_request_direction,
                    c.has_met,
                    c.friend_relay_consent,
                    COALESCE(p.presence_state, 'offline') AS presence_state,
                    p.last_seen_at_unix_ms,
                    p.last_sent_at_unix_ms,
                    p.last_activity_at_unix_ms,
                    c.removed_at_unix_ms,
                    c.created_at_unix_ms,
                    c.updated_at_unix_ms
                FROM contacts c
                LEFT JOIN contact_presence p
                  ON p.contact_id = c.contact_id
                 AND p.profile_id = c.profile_id
                WHERE c.remote_instance_id = ?1
                  AND c.profile_id = ?2
                  AND c.removed_at_unix_ms IS NULL
                ORDER BY c.updated_at_unix_ms DESC, c.contact_id ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![remote_instance_id, profile_id.0], map_contact_row)
            .map_err(map_sqlite_error)?;
        let mut contacts = Vec::new();
        for row in rows {
            let contact = row.map_err(map_sqlite_error)?;
            contacts.push(self.hydrate_contact_public_avatar_image_path(profile_id, contact));
        }
        Ok(contacts)
    }

    fn merge_candidate_contacts_for_profile(
        &self,
        profile_id: &LocalProfileId,
        candidates: Vec<ContactSnapshot>,
        now_unix_ms: i64,
    ) -> OmniResult<Option<ContactSnapshot>> {
        let mut deduped = Vec::new();
        let mut seen_contact_ids = std::collections::HashSet::new();
        for candidate in candidates {
            if seen_contact_ids.insert(candidate.contact_id.0.clone()) {
                deduped.push(candidate);
            }
        }

        if deduped.is_empty() {
            return Ok(None);
        }

        let distinct_remote_accounts: std::collections::BTreeSet<String> = deduped
            .iter()
            .filter_map(|candidate| candidate.remote_account_id.as_ref().map(|value| value.0.clone()))
            .collect();
        let Some(canonical_contact_id) =
            choose_canonical_contact(&deduped).map(|candidate| candidate.contact_id.clone())
        else {
            return Ok(None);
        };

        if deduped.len() == 1 || distinct_remote_accounts.len() > 1 {
            return self.get_contact_in_profile(profile_id, &canonical_contact_id);
        }

        self.with_immediate_transaction("merge_duplicate_contacts", |storage| {
            let Some(merged_snapshot) = merge_contact_snapshots(&deduped, now_unix_ms) else {
                return Ok(None);
            };
            storage.connection.execute(
                "
                UPDATE contacts
                SET petname = ?2,
                    remote_account_id = ?3,
                    public_display_name = ?4,
                    public_avatar_hash_hex = ?5,
                    public_avatar_mime_type = ?6,
                    public_avatar_pending_hash_hex = ?7,
                    public_avatar_pending_mime_type = ?8,
                    remote_instance_id = ?9,
                    relationship_state = ?10,
                    pending_request_direction = ?11,
                    has_met = ?12,
                    is_blocked = ?13,
                    friend_relay_consent = ?18,
                    removed_at_unix_ms = NULL,
                    public_profile_updated_at_unix_ms = ?14,
                    created_at_unix_ms = ?15,
                    updated_at_unix_ms = ?16
                WHERE contact_id = ?1
                  AND profile_id = ?17
                ",
                params![
                    merged_snapshot.contact_id.0,
                    merged_snapshot.petname,
                    merged_snapshot
                        .remote_account_id
                        .as_ref()
                        .map(|value| value.0.clone()),
                    merged_snapshot.public_display_name,
                    merged_snapshot.public_avatar_hash_hex,
                    merged_snapshot.public_avatar_mime_type,
                    merged_snapshot.public_avatar_pending_hash_hex,
                    merged_snapshot.public_avatar_pending_mime_type,
                    merged_snapshot.remote_instance_id,
                    merged_snapshot.relationship_state.as_db_value(),
                    merged_snapshot
                        .pending_request_direction
                        .map(|value| value.as_db_value()),
                    bool_to_sqlite_integer(merged_snapshot.has_met),
                    bool_to_sqlite_integer(merged_snapshot.is_blocked),
                    merged_snapshot.public_profile_updated_at_unix_ms,
                    merged_snapshot.created_at_unix_ms,
                    merged_snapshot.updated_at_unix_ms,
                    profile_id.0,
                    bool_to_sqlite_integer(merged_snapshot.friend_relay_consent),
                ],
            )
            .map_err(map_sqlite_error)?;

            for candidate in &deduped {
                if candidate.contact_id == merged_snapshot.contact_id {
                    continue;
                }

                storage.merge_contact_dialog_links_for_profile(
                    profile_id,
                    &merged_snapshot.contact_id,
                    &candidate.contact_id,
                )?;
                storage.merge_contact_verification_for_profile(
                    profile_id,
                    &merged_snapshot.contact_id,
                    &candidate.contact_id,
                )?;
                storage.merge_contact_identity_observation_for_profile(
                    profile_id,
                    &merged_snapshot.contact_id,
                    &candidate.contact_id,
                )?;
                storage.merge_contact_presence_for_profile(
                    profile_id,
                    &merged_snapshot.contact_id,
                    &candidate.contact_id,
                )?;
                storage.reassign_message_security_metadata_contact_id_for_profile(
                    profile_id,
                    &merged_snapshot.contact_id,
                    &candidate.contact_id,
                )?;
                storage.connection.execute(
                    "
                    DELETE FROM contacts
                    WHERE contact_id = ?1
                      AND profile_id = ?2
                    ",
                    params![candidate.contact_id.0, profile_id.0],
                )
                .map_err(map_sqlite_error)?;
            }

            storage.normalize_primary_dialog_link_for_contact_for_profile(
                profile_id,
                &merged_snapshot.contact_id,
            )?;

            storage.get_contact_in_profile(profile_id, &merged_snapshot.contact_id)
        })
    }

    fn find_dialog_keys_for_contact_in_profile(
        &self,
        profile_id: &LocalProfileId,
        contact_id: &ContactId,
    ) -> OmniResult<Vec<String>> {
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT dialog_key
                FROM contact_dialog_links
                WHERE contact_id = ?1
                  AND profile_id = ?2
                ORDER BY is_primary DESC, updated_at_unix_ms DESC, created_at_unix_ms ASC, dialog_key ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![contact_id.0, profile_id.0], |row| row.get::<_, String>(0))
            .map_err(map_sqlite_error)?;
        let mut dialog_keys = Vec::new();
        for row in rows {
            let dialog_key = omni_types::normalize_dialog_key(&row.map_err(map_sqlite_error)?);
            if !dialog_keys.contains(&dialog_key) {
                dialog_keys.push(dialog_key);
            }
        }
        Ok(dialog_keys)
    }

    fn normalize_primary_dialog_link_for_contact_for_profile(
        &self,
        profile_id: &LocalProfileId,
        contact_id: &ContactId,
    ) -> OmniResult<()> {
        let winner_dialog_key = self
            .connection
            .query_row(
                "
                SELECT dialog_key
                FROM contact_dialog_links
                WHERE contact_id = ?1
                  AND profile_id = ?2
                ORDER BY is_primary DESC, updated_at_unix_ms DESC, created_at_unix_ms ASC, dialog_key ASC
                LIMIT 1
                ",
                params![contact_id.0, profile_id.0],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;

        self.connection
            .execute(
                "
                UPDATE contact_dialog_links
                SET is_primary = 0
                WHERE contact_id = ?1
                  AND profile_id = ?2
                ",
                params![contact_id.0, profile_id.0],
            )
            .map_err(map_sqlite_error)?;

        if let Some(dialog_key) = winner_dialog_key {
            self.connection
                .execute(
                    "
                    UPDATE contact_dialog_links
                    SET is_primary = 1
                    WHERE contact_id = ?1
                      AND dialog_key = ?2
                      AND profile_id = ?3
                    ",
                    params![contact_id.0, dialog_key, profile_id.0],
                )
                .map_err(map_sqlite_error)?;
        }

        Ok(())
    }

    fn reassign_message_security_metadata_contact_id_for_profile(
        &self,
        profile_id: &LocalProfileId,
        canonical_contact_id: &ContactId,
        loser_contact_id: &ContactId,
    ) -> OmniResult<()> {
        self.connection
            .execute(
                "
                UPDATE message_security_metadata
                SET contact_id = ?1
                WHERE contact_id = ?2
                  AND profile_id = ?3
                ",
                params![canonical_contact_id.0, loser_contact_id.0, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    fn merge_contact_dialog_links_for_profile(
        &self,
        profile_id: &LocalProfileId,
        canonical_contact_id: &ContactId,
        loser_contact_id: &ContactId,
    ) -> OmniResult<()> {
        let mut statement = self
            .connection
            .prepare(
                "
                SELECT dialog_key, is_primary, created_at_unix_ms, updated_at_unix_ms
                FROM contact_dialog_links
                WHERE contact_id = ?1
                  AND profile_id = ?2
                ORDER BY is_primary DESC, updated_at_unix_ms DESC, dialog_key ASC
                ",
            )
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map(params![loser_contact_id.0, profile_id.0], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? != 0,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(map_sqlite_error)?;

        for row in rows {
            let (dialog_key, is_primary, created_at_unix_ms, updated_at_unix_ms) =
                row.map_err(map_sqlite_error)?;
            self.upsert_contact_dialog_link_for_profile(
                profile_id,
                canonical_contact_id,
                &dialog_key,
                is_primary,
                created_at_unix_ms,
                updated_at_unix_ms,
            )?;
            self.connection
                .execute(
                    "
                    UPDATE contact_dialog_links
                    SET created_at_unix_ms = CASE
                        WHEN created_at_unix_ms < ?3 THEN created_at_unix_ms
                        ELSE ?3
                    END
                    WHERE contact_id = ?1
                      AND dialog_key = ?2
                      AND profile_id = ?4
                    ",
                    params![
                        canonical_contact_id.0,
                        dialog_key,
                        created_at_unix_ms,
                        profile_id.0,
                    ],
                )
                .map_err(map_sqlite_error)?;
        }

        self.connection
            .execute(
                "
                DELETE FROM contact_dialog_links
                WHERE contact_id = ?1
                  AND profile_id = ?2
                ",
                params![loser_contact_id.0, profile_id.0],
            )
            .map_err(map_sqlite_error)?;

        Ok(())
    }

    fn merge_contact_verification_for_profile(
        &self,
        profile_id: &LocalProfileId,
        canonical_contact_id: &ContactId,
        loser_contact_id: &ContactId,
    ) -> OmniResult<()> {
        self.connection
            .execute(
                "
                INSERT INTO contact_verifications (
                    contact_id,
                    verified_at_unix_ms,
                    verification_method,
                    verified_identity_key,
                    updated_at_unix_ms,
                    profile_id
                )
                SELECT
                    ?1,
                    verified_at_unix_ms,
                    verification_method,
                    verified_identity_key,
                    updated_at_unix_ms,
                    profile_id
                FROM contact_verifications
                WHERE contact_id = ?2
                  AND profile_id = ?3
                ON CONFLICT(contact_id) DO UPDATE SET
                    verified_at_unix_ms = CASE WHEN contact_verifications.updated_at_unix_ms > excluded.updated_at_unix_ms THEN contact_verifications.verified_at_unix_ms ELSE excluded.verified_at_unix_ms END,
                    verification_method = CASE WHEN contact_verifications.updated_at_unix_ms > excluded.updated_at_unix_ms THEN contact_verifications.verification_method ELSE excluded.verification_method END,
                    verified_identity_key = CASE WHEN contact_verifications.updated_at_unix_ms > excluded.updated_at_unix_ms THEN contact_verifications.verified_identity_key ELSE excluded.verified_identity_key END,
                    updated_at_unix_ms = CASE WHEN contact_verifications.updated_at_unix_ms > excluded.updated_at_unix_ms THEN contact_verifications.updated_at_unix_ms ELSE excluded.updated_at_unix_ms END
                ",
                params![canonical_contact_id.0, loser_contact_id.0, profile_id.0],
            )
            .map_err(map_sqlite_error)?;

        self.connection
            .execute(
                "
                DELETE FROM contact_verifications
                WHERE contact_id = ?1
                  AND profile_id = ?2
                ",
                params![loser_contact_id.0, profile_id.0],
            )
            .map_err(map_sqlite_error)?;

        Ok(())
    }

    fn merge_contact_identity_observation_for_profile(
        &self,
        profile_id: &LocalProfileId,
        canonical_contact_id: &ContactId,
        loser_contact_id: &ContactId,
    ) -> OmniResult<()> {
        self.connection
            .execute(
                "
                INSERT INTO contact_identity_observations (
                    contact_id,
                    identity_key,
                    observed_display_name,
                    observed_at_unix_ms,
                    updated_at_unix_ms,
                    profile_id
                )
                SELECT
                    ?1,
                    identity_key,
                    observed_display_name,
                    observed_at_unix_ms,
                    updated_at_unix_ms,
                    profile_id
                FROM contact_identity_observations
                WHERE contact_id = ?2
                  AND profile_id = ?3
                ON CONFLICT(contact_id) DO UPDATE SET
                    identity_key = CASE WHEN contact_identity_observations.updated_at_unix_ms > excluded.updated_at_unix_ms THEN contact_identity_observations.identity_key ELSE excluded.identity_key END,
                    observed_display_name = CASE WHEN contact_identity_observations.updated_at_unix_ms > excluded.updated_at_unix_ms THEN contact_identity_observations.observed_display_name ELSE excluded.observed_display_name END,
                    observed_at_unix_ms = CASE WHEN contact_identity_observations.updated_at_unix_ms > excluded.updated_at_unix_ms THEN contact_identity_observations.observed_at_unix_ms ELSE excluded.observed_at_unix_ms END,
                    updated_at_unix_ms = CASE WHEN contact_identity_observations.updated_at_unix_ms > excluded.updated_at_unix_ms THEN contact_identity_observations.updated_at_unix_ms ELSE excluded.updated_at_unix_ms END
                ",
                params![canonical_contact_id.0, loser_contact_id.0, profile_id.0],
            )
            .map_err(map_sqlite_error)?;

        self.connection
            .execute(
                "
                DELETE FROM contact_identity_observations
                WHERE contact_id = ?1
                  AND profile_id = ?2
                ",
                params![loser_contact_id.0, profile_id.0],
            )
            .map_err(map_sqlite_error)?;

        Ok(())
    }

    fn merge_contact_presence_for_profile(
        &self,
        profile_id: &LocalProfileId,
        canonical_contact_id: &ContactId,
        loser_contact_id: &ContactId,
    ) -> OmniResult<()> {
        let loser_presence = self
            .connection
            .query_row(
                "
                SELECT
                    last_seen_at_unix_ms,
                    last_sent_at_unix_ms,
                    last_activity_at_unix_ms,
                    presence_state,
                    consecutive_missed_heartbeats,
                    next_heartbeat_due_at_unix_ms,
                    created_at_unix_ms,
                    updated_at_unix_ms
                FROM contact_presence
                WHERE contact_id = ?1
                  AND profile_id = ?2
                ",
                params![loser_contact_id.0, profile_id.0],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, i64>(7)?,
                    ))
                },
            )
            .optional()
            .map_err(map_sqlite_error)?;

        let Some((
            loser_last_seen,
            loser_last_sent,
            loser_last_activity,
            loser_presence_state,
            loser_miss_count,
            loser_next_due,
            loser_created_at,
            loser_updated_at,
        )) = loser_presence
        else {
            return Ok(());
        };

        let canonical_presence = self
            .connection
            .query_row(
                "
                SELECT
                    last_seen_at_unix_ms,
                    last_sent_at_unix_ms,
                    last_activity_at_unix_ms,
                    presence_state,
                    consecutive_missed_heartbeats,
                    next_heartbeat_due_at_unix_ms,
                    created_at_unix_ms,
                    updated_at_unix_ms
                FROM contact_presence
                WHERE contact_id = ?1
                  AND profile_id = ?2
                ",
                params![canonical_contact_id.0, profile_id.0],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, i64>(7)?,
                    ))
                },
            )
            .optional()
            .map_err(map_sqlite_error)?;

        let (
            canonical_last_seen,
            canonical_last_sent,
            canonical_last_activity,
            canonical_presence_state,
            canonical_miss_count,
            canonical_next_due,
            canonical_created_at,
            canonical_updated_at,
        ) = canonical_presence.unwrap_or((
            None,
            None,
            None,
            omni_identity::ContactPresenceState::Offline.as_db_value().to_string(),
            0,
            None,
            loser_created_at,
            loser_updated_at,
        ));

        self.connection
            .execute(
                "
                INSERT INTO contact_presence (
                    contact_id,
                    profile_id,
                    last_seen_at_unix_ms,
                    last_sent_at_unix_ms,
                    last_activity_at_unix_ms,
                    presence_state,
                    consecutive_missed_heartbeats,
                    next_heartbeat_due_at_unix_ms,
                    created_at_unix_ms,
                    updated_at_unix_ms
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                ON CONFLICT(contact_id, profile_id) DO UPDATE SET
                    last_seen_at_unix_ms = excluded.last_seen_at_unix_ms,
                    last_sent_at_unix_ms = excluded.last_sent_at_unix_ms,
                    last_activity_at_unix_ms = excluded.last_activity_at_unix_ms,
                    presence_state = excluded.presence_state,
                    consecutive_missed_heartbeats = excluded.consecutive_missed_heartbeats,
                    next_heartbeat_due_at_unix_ms = excluded.next_heartbeat_due_at_unix_ms,
                    created_at_unix_ms = excluded.created_at_unix_ms,
                    updated_at_unix_ms = excluded.updated_at_unix_ms
                ",
                params![
                    canonical_contact_id.0,
                    profile_id.0,
                    pick_latest_optional_i64(canonical_last_seen, loser_last_seen),
                    pick_latest_optional_i64(canonical_last_sent, loser_last_sent),
                    pick_latest_optional_i64(canonical_last_activity, loser_last_activity),
                    if loser_updated_at >= canonical_updated_at {
                        loser_presence_state
                    } else {
                        canonical_presence_state
                    },
                    canonical_miss_count.max(loser_miss_count),
                    pick_latest_optional_i64(canonical_next_due, loser_next_due),
                    canonical_created_at.min(loser_created_at),
                    canonical_updated_at.max(loser_updated_at),
                ],
            )
            .map_err(map_sqlite_error)?;

        self.connection
            .execute(
                "
                DELETE FROM contact_presence
                WHERE contact_id = ?1
                  AND profile_id = ?2
                ",
                params![loser_contact_id.0, profile_id.0],
            )
            .map_err(map_sqlite_error)?;

        Ok(())
    }
}
