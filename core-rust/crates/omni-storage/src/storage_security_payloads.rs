impl Storage {
    pub fn record_remote_prekey_bundle(
        &self,
        bundle: &PrekeyBundle,
        account_display_name: Option<&str>,
        device_label: Option<&str>,
        observed_at_unix_ms: i64,
    ) -> OmniResult<()> {
        let bundle_key = format!("{}::{}", bundle.account_id.0, bundle.device_id.0);

        // The account's working key moves forward only: a chain certified by an older key than one
        // we have seen is a device the owner left behind, and a newer one means the account moved.
        if let Some(chain) = bundle.device_certificate_chain.as_ref() {
            self.observe_working_key_certificate(&chain.working, observed_at_unix_ms)?;
            if self
                .current_working_key_version(&bundle.account_id)?
                .is_some_and(|current| chain.working.version < current)
            {
                return Err(OmniError::InvalidInput(
                    "the device is certified by a superseded working key".to_string(),
                ));
            }
            self.clear_superseded(&bundle.account_id, &bundle.device_id)?;
        }


        self.connection
            .execute(
                "
                INSERT INTO remote_device_observations (
                    remote_device_id,
                    remote_account_id,
                    device_public_key,
                    device_label,
                    observed_at_unix_ms,
                    updated_at_unix_ms
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ON CONFLICT(remote_device_id) DO UPDATE SET
                    remote_account_id = excluded.remote_account_id,
                    device_public_key = excluded.device_public_key,
                    device_label = excluded.device_label,
                    observed_at_unix_ms = excluded.observed_at_unix_ms,
                    updated_at_unix_ms = excluded.updated_at_unix_ms
                ",
                params![
                    bundle.device_id.0,
                    bundle.account_id.0,
                    device_public_key_to_wire(bundle.device_key),
                    device_label,
                    observed_at_unix_ms,
                    observed_at_unix_ms,
                ],
            )
            .map_err(map_sqlite_error)?;

        let (signature_r, signature_s) =
            signature_to_wire(bundle.signed_prekey.signature.clone());
        self.connection
            .execute(
                "
                INSERT INTO remote_prekey_bundles (
                    bundle_key,
                    remote_account_id,
                    remote_device_id,
                    account_display_name,
                    device_label,
                    identity_public_key,
                    identity_signing_public_key,
                    device_public_key,
                    signed_prekey_id,
                    signed_prekey_public_key,
                    signed_prekey_signature_r,
                    signed_prekey_signature_s,
                    one_time_prekey_id,
                    one_time_prekey_public_key,
                    generated_at_unix_ms,
                    observed_at_unix_ms,
                    updated_at_unix_ms,
                    version,
                    device_certificate_chain_json
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
                ON CONFLICT(bundle_key) DO UPDATE SET
                    account_display_name = excluded.account_display_name,
                    device_label = excluded.device_label,
                    identity_public_key = excluded.identity_public_key,
                    identity_signing_public_key = excluded.identity_signing_public_key,
                    device_public_key = excluded.device_public_key,
                    signed_prekey_id = excluded.signed_prekey_id,
                    signed_prekey_public_key = excluded.signed_prekey_public_key,
                    signed_prekey_signature_r = excluded.signed_prekey_signature_r,
                    signed_prekey_signature_s = excluded.signed_prekey_signature_s,
                    one_time_prekey_id = excluded.one_time_prekey_id,
                    one_time_prekey_public_key = excluded.one_time_prekey_public_key,
                    generated_at_unix_ms = excluded.generated_at_unix_ms,
                    observed_at_unix_ms = excluded.observed_at_unix_ms,
                    updated_at_unix_ms = excluded.updated_at_unix_ms,
                    version = excluded.version,
                    device_certificate_chain_json = COALESCE(
                        excluded.device_certificate_chain_json,
                        device_certificate_chain_json
                    )
                ",
                params![
                    bundle_key,
                    bundle.account_id.0,
                    bundle.device_id.0,
                    account_display_name,
                    device_label,
                    identity_public_key_to_wire(bundle.identity_key),
                    identity_signing_public_key_to_wire(bundle.identity_signing_key),
                    device_public_key_to_wire(bundle.device_key),
                    bundle.signed_prekey.prekey_id as i64,
                    prekey_public_key_to_wire(bundle.signed_prekey.public_key),
                    signature_r,
                    signature_s,
                    bundle.one_time_prekey.as_ref().map(|value| value.prekey_id as i64),
                    bundle
                        .one_time_prekey
                        .as_ref()
                        .map(|value| prekey_public_key_to_wire(value.public_key)),
                    bundle.generated_at_unix_ms,
                    observed_at_unix_ms,
                    observed_at_unix_ms,
                    bundle.suite_version as i64,
                    bundle
                        .device_certificate_chain
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()
                        .map_err(map_serde_error_to_storage)?,
                ],
            )
            .map_err(map_sqlite_error)?;

        Ok(())
    }

    pub fn get_remote_prekey_bundle(
        &self,
        remote_account_id: &AccountId,
        remote_device_id: &DeviceId,
    ) -> OmniResult<Option<PrekeyBundle>> {
        let bundle_key = format!("{}::{}", remote_account_id.0, remote_device_id.0);

        self.connection
            .query_row(
                "
                SELECT
                    version,
                    remote_account_id,
                    remote_device_id,
                    identity_public_key,
                    identity_signing_public_key,
                    device_public_key,
                    signed_prekey_id,
                    signed_prekey_public_key,
                    signed_prekey_signature_r,
                    signed_prekey_signature_s,
                    one_time_prekey_id,
                    one_time_prekey_public_key,
                    generated_at_unix_ms,
                    device_certificate_chain_json
                FROM remote_prekey_bundles
                WHERE bundle_key = ?1
                ",
                params![bundle_key],
                |row| {
                    Ok(PrekeyBundle {
                        suite_version: row.get::<_, i64>(0)? as u16,
                        account_id: AccountId(row.get(1)?),
                        device_id: DeviceId(row.get(2)?),
                        identity_key: identity_public_key_from_wire(&row.get::<_, String>(3)?)
                            .map_err(to_sqlite_user_error)?,
                        identity_signing_key: identity_signing_public_key_from_wire(
                            &row.get::<_, String>(4)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        device_key: device_public_key_from_wire(&row.get::<_, String>(5)?)
                            .map_err(to_sqlite_user_error)?,
                        signed_prekey: omni_crypto::SignedPrekeyDescriptor {
                            prekey_id: row.get::<_, i64>(6)? as u32,
                            public_key: prekey_public_key_from_wire(&row.get::<_, String>(7)?)
                                .map_err(to_sqlite_user_error)?,
                            signature: signature_from_wire(
                                &row.get::<_, String>(8)?,
                                &row.get::<_, String>(9)?,
                            )
                            .map_err(to_sqlite_user_error)?,
                            expires_at_unix_ms: None,
                        },
                        one_time_prekey: {
                            let prekey_id = row.get::<_, Option<i64>>(10)?;
                            let public_key = row.get::<_, Option<String>>(11)?;
                            match (prekey_id, public_key) {
                                (Some(prekey_id), Some(public_key)) => {
                                    Some(omni_crypto::OneTimePrekeyDescriptor {
                                        prekey_id: prekey_id as u32,
                                        public_key: prekey_public_key_from_wire(&public_key)
                                            .map_err(to_sqlite_user_error)?,
                                    })
                                }
                                _ => None,
                            }
                        },
                        generated_at_unix_ms: row.get(12)?,
                        device_certificate_chain: row
                            .get::<_, Option<String>>(13)?
                            .and_then(|json| serde_json::from_str(&json).ok()),
                    })
                },
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    pub fn get_latest_remote_prekey_bundle_for_account(
        &self,
        remote_account_id: &AccountId,
    ) -> OmniResult<Option<PrekeyBundle>> {
        self.connection
            .query_row(
                "
                SELECT
                    version,
                    remote_account_id,
                    remote_device_id,
                    identity_public_key,
                    identity_signing_public_key,
                    device_public_key,
                    signed_prekey_id,
                    signed_prekey_public_key,
                    signed_prekey_signature_r,
                    signed_prekey_signature_s,
                    one_time_prekey_id,
                    one_time_prekey_public_key,
                    generated_at_unix_ms,
                    device_certificate_chain_json
                FROM remote_prekey_bundles
                WHERE remote_account_id = ?1
                ORDER BY observed_at_unix_ms DESC, generated_at_unix_ms DESC
                LIMIT 1
                ",
                params![remote_account_id.0],
                |row| {
                    Ok(PrekeyBundle {
                        suite_version: row.get::<_, i64>(0)? as u16,
                        account_id: AccountId(row.get(1)?),
                        device_id: DeviceId(row.get(2)?),
                        identity_key: identity_public_key_from_wire(&row.get::<_, String>(3)?)
                            .map_err(to_sqlite_user_error)?,
                        identity_signing_key: identity_signing_public_key_from_wire(
                            &row.get::<_, String>(4)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        device_key: device_public_key_from_wire(&row.get::<_, String>(5)?)
                            .map_err(to_sqlite_user_error)?,
                        signed_prekey: omni_crypto::SignedPrekeyDescriptor {
                            prekey_id: row.get::<_, i64>(6)? as u32,
                            public_key: prekey_public_key_from_wire(&row.get::<_, String>(7)?)
                                .map_err(to_sqlite_user_error)?,
                            signature: signature_from_wire(
                                &row.get::<_, String>(8)?,
                                &row.get::<_, String>(9)?,
                            )
                            .map_err(to_sqlite_user_error)?,
                            expires_at_unix_ms: None,
                        },
                        one_time_prekey: {
                            let prekey_id = row.get::<_, Option<i64>>(10)?;
                            let public_key = row.get::<_, Option<String>>(11)?;
                            match (prekey_id, public_key) {
                                (Some(prekey_id), Some(public_key)) => {
                                    Some(omni_crypto::OneTimePrekeyDescriptor {
                                        prekey_id: prekey_id as u32,
                                        public_key: prekey_public_key_from_wire(&public_key)
                                            .map_err(to_sqlite_user_error)?,
                                    })
                                }
                                _ => None,
                            }
                        },
                        generated_at_unix_ms: row.get(12)?,
                        device_certificate_chain: row
                            .get::<_, Option<String>>(13)?
                            .and_then(|json| serde_json::from_str(&json).ok()),
                    })
                },
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    pub fn record_remote_device_observation(
        &self,
        remote_account_id: &AccountId,
        remote_device_id: &DeviceId,
        device_public_key: &str,
        observed_at_unix_ms: i64,
    ) -> OmniResult<()> {
        self.connection
            .execute(
                "
                INSERT INTO remote_device_observations (
                    remote_device_id,
                    remote_account_id,
                    device_public_key,
                    device_label,
                    observed_at_unix_ms,
                    updated_at_unix_ms
                )
                VALUES (?1, ?2, ?3, NULL, ?4, ?5)
                ON CONFLICT(remote_device_id) DO UPDATE SET
                    remote_account_id = excluded.remote_account_id,
                    device_public_key = excluded.device_public_key,
                    observed_at_unix_ms = excluded.observed_at_unix_ms,
                    updated_at_unix_ms = excluded.updated_at_unix_ms
                ",
                params![
                    remote_device_id.0,
                    remote_account_id.0,
                    device_public_key,
                    observed_at_unix_ms,
                    observed_at_unix_ms,
                ],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    /// Write back a session this copy was READ from storage as (or one never stored at all), and
    /// only if nobody has written it since: the stored revision must equal `session.revision`.
    /// On success `session.revision` becomes the new stored revision, so the same copy can be
    /// advanced and written again. A copy that went stale — read before another path advanced the
    /// ratchet, or a just-built session where one already exists — is refused with
    /// [`STALE_SESSION_WRITE`] and nothing is written: writing it would roll the ratchet back and
    /// split the two peers for good (docs/bugs/stale-session-copy-diverges-ratchet.md). The caller
    /// must not send what it encrypted with that copy.
    pub fn store_session_record(&self, session: &mut SessionRecord) -> OmniResult<()> {
        self.write_session_record(session, Some(session.revision))
    }

    /// Install a NEW session for its address, whatever is stored there: a bootstrap that
    /// deliberately supersedes the old session (fresh initiator, accepted responder, rebuild).
    /// The only write that ignores the revision — everything that ADVANCES a session uses
    /// [`store_session_record`](Self::store_session_record).
    pub fn replace_session_record(&self, session: &mut SessionRecord) -> OmniResult<()> {
        self.write_session_record(session, None)
    }

    fn write_session_record(
        &self,
        session: &mut SessionRecord,
        expected_revision: Option<u64>,
    ) -> OmniResult<()> {
        let profile_id = self.resolve_transition_profile_id()?;
        // One statement, so the check and the write cannot interleave with another connection's.
        // `?35` = the revision the copy descends from, or -1 to replace unconditionally. Stored
        // revisions start at 1, so a never-stored copy (0) only ever inserts.
        let written = self
            .connection
            .query_row(
                "
                INSERT INTO sessions (
                    session_address,
                    remote_account_id,
                    remote_device_id,
                    local_account_id,
                    local_device_id,
                    local_identity_key,
                    local_identity_signing_key,
                    local_device_key,
                    remote_identity_key,
                    remote_identity_signing_key,
                    remote_device_key,
                    session_key_protection_scheme,
                    session_key_blob,
                    send_chain_key_protection_scheme,
                    send_chain_key_blob,
                    receive_chain_key_protection_scheme,
                    receive_chain_key_blob,
                    local_ratchet_key_protection_scheme,
                    local_ratchet_key_blob,
                    local_ratchet_public_key,
                    remote_ratchet_public_key,
                    bootstrap_state,
                    send_counter,
                    receive_counter,
                    previous_send_chain_length,
                    needs_send_ratchet,
                    remote_signed_prekey_id,
                    remote_one_time_prekey_id,
                    suite_version,
                    created_at_unix_ms,
                    updated_at_unix_ms,
                    profile_id,
                    rendezvous_secret_protection_scheme,
                    rendezvous_secret_blob,
                    skipped_message_keys_protection_scheme,
                    skipped_message_keys_blob,
                    revision
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?36, ?37, 1)
                ON CONFLICT(session_address) DO UPDATE SET
                    remote_account_id = excluded.remote_account_id,
                    remote_device_id = excluded.remote_device_id,
                    local_account_id = excluded.local_account_id,
                    local_device_id = excluded.local_device_id,
                    local_identity_key = excluded.local_identity_key,
                    local_identity_signing_key = excluded.local_identity_signing_key,
                    local_device_key = excluded.local_device_key,
                    remote_identity_key = excluded.remote_identity_key,
                    remote_identity_signing_key = excluded.remote_identity_signing_key,
                    remote_device_key = excluded.remote_device_key,
                    session_key_protection_scheme = excluded.session_key_protection_scheme,
                    session_key_blob = excluded.session_key_blob,
                    send_chain_key_protection_scheme = excluded.send_chain_key_protection_scheme,
                    send_chain_key_blob = excluded.send_chain_key_blob,
                    receive_chain_key_protection_scheme = excluded.receive_chain_key_protection_scheme,
                    receive_chain_key_blob = excluded.receive_chain_key_blob,
                    local_ratchet_key_protection_scheme = excluded.local_ratchet_key_protection_scheme,
                    local_ratchet_key_blob = excluded.local_ratchet_key_blob,
                    local_ratchet_public_key = excluded.local_ratchet_public_key,
                    remote_ratchet_public_key = excluded.remote_ratchet_public_key,
                    bootstrap_state = excluded.bootstrap_state,
                    send_counter = excluded.send_counter,
                    receive_counter = excluded.receive_counter,
                    previous_send_chain_length = excluded.previous_send_chain_length,
                    needs_send_ratchet = excluded.needs_send_ratchet,
                    remote_signed_prekey_id = excluded.remote_signed_prekey_id,
                    remote_one_time_prekey_id = excluded.remote_one_time_prekey_id,
                    suite_version = excluded.suite_version,
                    updated_at_unix_ms = excluded.updated_at_unix_ms,
                    rendezvous_secret_protection_scheme = excluded.rendezvous_secret_protection_scheme,
                    rendezvous_secret_blob = excluded.rendezvous_secret_blob,
                    skipped_message_keys_protection_scheme = excluded.skipped_message_keys_protection_scheme,
                    skipped_message_keys_blob = excluded.skipped_message_keys_blob,
                    revision = sessions.revision + 1
                WHERE ?35 < 0 OR sessions.revision = ?35
                RETURNING revision
                ",
                params![
                    session.session_address.storage_key(),
                    session.session_address.remote_account_id.0,
                    session.session_address.remote_device_id.0,
                    session.local_account_id.0,
                    session.local_device_id.0,
                    identity_public_key_to_wire(session.local_identity_key),
                    identity_signing_public_key_to_wire(session.local_identity_signing_key),
                    device_public_key_to_wire(session.local_device_key),
                    identity_public_key_to_wire(session.remote_identity_key),
                    identity_signing_public_key_to_wire(session.remote_identity_signing_key),
                    device_public_key_to_wire(session.remote_device_key),
                    session.protected_session_key.scheme.as_db_value(),
                    session.protected_session_key.blob_hex,
                    session
                        .protected_send_chain_key
                        .as_ref()
                        .map(|value| value.scheme.as_db_value().to_string()),
                    session
                        .protected_send_chain_key
                        .as_ref()
                        .map(|value| value.blob_hex.clone()),
                    session
                        .protected_receive_chain_key
                        .as_ref()
                        .map(|value| value.scheme.as_db_value().to_string()),
                    session
                        .protected_receive_chain_key
                        .as_ref()
                        .map(|value| value.blob_hex.clone()),
                    session.protected_local_ratchet_key.scheme.as_db_value(),
                    session.protected_local_ratchet_key.blob_hex.clone(),
                    ephemeral_public_key_to_wire(session.local_ratchet_public_key),
                    ephemeral_public_key_to_wire(session.remote_ratchet_public_key),
                    session.bootstrap_state.as_db_value(),
                    session.send_counter as i64,
                    session.receive_counter as i64,
                    session.previous_send_chain_length as i64,
                    bool_to_sqlite_integer(session.needs_send_ratchet),
                    session.remote_signed_prekey_id as i64,
                    session.remote_one_time_prekey_id.map(|value| value as i64),
                    session.suite_version as i64,
                    session.created_at_unix_ms,
                    session.updated_at_unix_ms,
                    profile_id.0,
                    session
                        .protected_rendezvous_secret
                        .as_ref()
                        .map(|value| value.scheme.as_db_value().to_string()),
                    session
                        .protected_rendezvous_secret
                        .as_ref()
                        .map(|value| value.blob_hex.clone()),
                    expected_revision.map_or(-1, |revision| revision as i64),
                    session
                        .protected_skipped_message_keys
                        .as_ref()
                        .map(|value| value.scheme.as_db_value().to_string()),
                    session
                        .protected_skipped_message_keys
                        .as_ref()
                        .map(|value| value.blob_hex.clone()),
                ],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        let Some(written) = written else {
            return Err(OmniError::Storage(format!(
                "{STALE_SESSION_WRITE}: session {} was written since this copy (revision {}) was read",
                session.session_address.storage_key(),
                session.revision
            )));
        };
        session.revision = written as u64;
        Ok(())
    }

    pub fn get_session_record(
        &self,
        session_address: &SessionAddress,
    ) -> OmniResult<Option<SessionRecord>> {
        let profile_id = self.resolve_transition_profile_id()?;
        self.connection
            .query_row(
                "
                SELECT
                    remote_account_id,
                    remote_device_id,
                    local_account_id,
                    local_device_id,
                    local_identity_key,
                    local_identity_signing_key,
                    local_device_key,
                    remote_identity_key,
                    remote_identity_signing_key,
                    remote_device_key,
                    session_key_protection_scheme,
                    session_key_blob,
                    send_chain_key_protection_scheme,
                    send_chain_key_blob,
                    receive_chain_key_protection_scheme,
                    receive_chain_key_blob,
                    local_ratchet_key_protection_scheme,
                    local_ratchet_key_blob,
                    local_ratchet_public_key,
                    remote_ratchet_public_key,
                    bootstrap_state,
                    send_counter,
                    receive_counter,
                    previous_send_chain_length,
                    needs_send_ratchet,
                    remote_signed_prekey_id,
                    remote_one_time_prekey_id,
                    suite_version,
                    created_at_unix_ms,
                    updated_at_unix_ms,
                    rendezvous_secret_protection_scheme,
                    rendezvous_secret_blob,
                    revision,
                    skipped_message_keys_protection_scheme,
                    skipped_message_keys_blob
                FROM sessions
                WHERE session_address = ?1
                  AND profile_id = ?2
                ",
                params![session_address.storage_key(), profile_id.0],
                |row| {
                    Ok(SessionRecord {
                        session_address: SessionAddress {
                            remote_account_id: AccountId(row.get(0)?),
                            remote_device_id: DeviceId(row.get(1)?),
                        },
                        local_account_id: AccountId(row.get(2)?),
                        local_device_id: DeviceId(row.get(3)?),
                        local_identity_key: identity_public_key_from_wire(&row.get::<_, String>(4)?)
                            .map_err(to_sqlite_user_error)?,
                        local_identity_signing_key: identity_signing_public_key_from_wire(
                            &row.get::<_, String>(5)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        local_device_key: device_public_key_from_wire(&row.get::<_, String>(6)?)
                            .map_err(to_sqlite_user_error)?,
                        remote_identity_key: identity_public_key_from_wire(&row.get::<_, String>(7)?)
                            .map_err(to_sqlite_user_error)?,
                        remote_identity_signing_key: identity_signing_public_key_from_wire(
                            &row.get::<_, String>(8)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        remote_device_key: device_public_key_from_wire(&row.get::<_, String>(9)?)
                            .map_err(to_sqlite_user_error)?,
                        protected_session_key: protected_secret_from_db(
                            &row.get::<_, String>(10)?,
                            &row.get::<_, String>(11)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        protected_send_chain_key: protected_secret_from_optional_db(
                            row.get::<_, Option<String>>(12)?,
                            row.get::<_, Option<String>>(13)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        protected_receive_chain_key: protected_secret_from_optional_db(
                            row.get::<_, Option<String>>(14)?,
                            row.get::<_, Option<String>>(15)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        protected_local_ratchet_key: protected_secret_from_db(
                            &row.get::<_, String>(16)?,
                            &row.get::<_, String>(17)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        local_ratchet_public_key: ephemeral_public_key_from_wire(
                            &row.get::<_, String>(18)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        remote_ratchet_public_key: ephemeral_public_key_from_wire(
                            &row.get::<_, String>(19)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        bootstrap_state: SessionBootstrapState::from_db_value(
                            &row.get::<_, String>(20)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        send_counter: row.get::<_, i64>(21)? as u64,
                        receive_counter: row.get::<_, i64>(22)? as u64,
                        previous_send_chain_length: row.get::<_, i64>(23)? as u64,
                        needs_send_ratchet: row.get::<_, i64>(24)? != 0,
                        remote_signed_prekey_id: row.get::<_, i64>(25)? as u32,
                        remote_one_time_prekey_id: row.get::<_, Option<i64>>(26)?
                            .map(|value| value as u32),
                        suite_version: row.get::<_, i64>(27)? as u16,
                        created_at_unix_ms: row.get(28)?,
                        updated_at_unix_ms: row.get(29)?,
                        protected_rendezvous_secret: protected_secret_from_optional_db(
                            row.get::<_, Option<String>>(30)?,
                            row.get::<_, Option<String>>(31)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        revision: row.get::<_, i64>(32)? as u64,
                        protected_skipped_message_keys: protected_secret_from_optional_db(
                            row.get::<_, Option<String>>(33)?,
                            row.get::<_, Option<String>>(34)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                    })
                },
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    pub fn delete_session_state_for_remote_account(
        &self,
        remote_account_id: &AccountId,
    ) -> OmniResult<()> {
        let profile_id = self.resolve_transition_profile_id()?;
        self.connection
            .execute(
                "
                DELETE FROM replay_windows
                WHERE profile_id = ?2
                  AND session_address IN (
                      SELECT session_address
                      FROM sessions
                      WHERE remote_account_id = ?1
                        AND profile_id = ?2
                  )
                ",
                params![remote_account_id.0, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                DELETE FROM sessions
                WHERE remote_account_id = ?1
                  AND profile_id = ?2
                ",
                params![remote_account_id.0, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn delete_session_state_for_session_address(
        &self,
        session_address: &SessionAddress,
    ) -> OmniResult<()> {
        let profile_id = self.resolve_transition_profile_id()?;
        self.connection
            .execute(
                "
                DELETE FROM replay_windows
                WHERE profile_id = ?2
                  AND session_address = ?1
                ",
                params![session_address.storage_key(), profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                DELETE FROM sessions
                WHERE session_address = ?1
                  AND profile_id = ?2
                ",
                params![session_address.storage_key(), profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn delete_replay_windows_for_remote_account(
        &self,
        remote_account_id: &AccountId,
    ) -> OmniResult<()> {
        let profile_id = self.resolve_transition_profile_id()?;
        self.connection
            .execute(
                "
                DELETE FROM replay_windows
                WHERE profile_id = ?2
                  AND session_address IN (
                      SELECT session_address
                      FROM sessions
                      WHERE remote_account_id = ?1
                        AND profile_id = ?2
                  )
                ",
                params![remote_account_id.0, profile_id.0],
            )
            .map_err(map_sqlite_error)?;
        Ok(())
    }

    pub fn get_latest_session_for_remote_account(
        &self,
        remote_account_id: &AccountId,
    ) -> OmniResult<Option<SessionRecord>> {
        let profile_id = self.resolve_transition_profile_id()?;
        self.connection
            .query_row(
                "
                SELECT
                    remote_account_id,
                    remote_device_id,
                    local_account_id,
                    local_device_id,
                    local_identity_key,
                    local_identity_signing_key,
                    local_device_key,
                    remote_identity_key,
                    remote_identity_signing_key,
                    remote_device_key,
                    session_key_protection_scheme,
                    session_key_blob,
                    send_chain_key_protection_scheme,
                    send_chain_key_blob,
                    receive_chain_key_protection_scheme,
                    receive_chain_key_blob,
                    local_ratchet_key_protection_scheme,
                    local_ratchet_key_blob,
                    local_ratchet_public_key,
                    remote_ratchet_public_key,
                    bootstrap_state,
                    send_counter,
                    receive_counter,
                    previous_send_chain_length,
                    needs_send_ratchet,
                    remote_signed_prekey_id,
                    remote_one_time_prekey_id,
                    suite_version,
                    created_at_unix_ms,
                    updated_at_unix_ms,
                    rendezvous_secret_protection_scheme,
                    rendezvous_secret_blob,
                    revision,
                    skipped_message_keys_protection_scheme,
                    skipped_message_keys_blob
                FROM sessions
                WHERE remote_account_id = ?1
                  AND profile_id = ?2
                ORDER BY updated_at_unix_ms DESC
                LIMIT 1
                ",
                params![remote_account_id.0, profile_id.0],
                |row| {
                    Ok(SessionRecord {
                        session_address: SessionAddress {
                            remote_account_id: AccountId(row.get(0)?),
                            remote_device_id: DeviceId(row.get(1)?),
                        },
                        local_account_id: AccountId(row.get(2)?),
                        local_device_id: DeviceId(row.get(3)?),
                        local_identity_key: identity_public_key_from_wire(&row.get::<_, String>(4)?)
                            .map_err(to_sqlite_user_error)?,
                        local_identity_signing_key: identity_signing_public_key_from_wire(
                            &row.get::<_, String>(5)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        local_device_key: device_public_key_from_wire(&row.get::<_, String>(6)?)
                            .map_err(to_sqlite_user_error)?,
                        remote_identity_key: identity_public_key_from_wire(&row.get::<_, String>(7)?)
                            .map_err(to_sqlite_user_error)?,
                        remote_identity_signing_key: identity_signing_public_key_from_wire(
                            &row.get::<_, String>(8)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        remote_device_key: device_public_key_from_wire(&row.get::<_, String>(9)?)
                            .map_err(to_sqlite_user_error)?,
                        protected_session_key: protected_secret_from_db(
                            &row.get::<_, String>(10)?,
                            &row.get::<_, String>(11)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        protected_send_chain_key: protected_secret_from_optional_db(
                            row.get::<_, Option<String>>(12)?,
                            row.get::<_, Option<String>>(13)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        protected_receive_chain_key: protected_secret_from_optional_db(
                            row.get::<_, Option<String>>(14)?,
                            row.get::<_, Option<String>>(15)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        protected_local_ratchet_key: protected_secret_from_db(
                            &row.get::<_, String>(16)?,
                            &row.get::<_, String>(17)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        local_ratchet_public_key: ephemeral_public_key_from_wire(
                            &row.get::<_, String>(18)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        remote_ratchet_public_key: ephemeral_public_key_from_wire(
                            &row.get::<_, String>(19)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        bootstrap_state: SessionBootstrapState::from_db_value(
                            &row.get::<_, String>(20)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        send_counter: row.get::<_, i64>(21)? as u64,
                        receive_counter: row.get::<_, i64>(22)? as u64,
                        previous_send_chain_length: row.get::<_, i64>(23)? as u64,
                        needs_send_ratchet: row.get::<_, i64>(24)? != 0,
                        remote_signed_prekey_id: row.get::<_, i64>(25)? as u32,
                        remote_one_time_prekey_id: row.get::<_, Option<i64>>(26)?
                            .map(|value| value as u32),
                        suite_version: row.get::<_, i64>(27)? as u16,
                        created_at_unix_ms: row.get(28)?,
                        updated_at_unix_ms: row.get(29)?,
                        protected_rendezvous_secret: protected_secret_from_optional_db(
                            row.get::<_, Option<String>>(30)?,
                            row.get::<_, Option<String>>(31)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                        revision: row.get::<_, i64>(32)? as u64,
                        protected_skipped_message_keys: protected_secret_from_optional_db(
                            row.get::<_, Option<String>>(33)?,
                            row.get::<_, Option<String>>(34)?,
                        )
                        .map_err(to_sqlite_user_error)?,
                    })
                },
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    pub fn store_replay_window(&self, replay_window: &ReplayWindowState) -> OmniResult<()> {
        let profile_id = self.resolve_transition_profile_id()?;
        self.connection
            .execute(
                "
                INSERT INTO replay_windows (
                    session_address,
                    highest_counter,
                    recent_tags_json,
                    updated_at_unix_ms,
                    profile_id
                )
                VALUES (?1, ?2, ?3, ?4, ?5)
                ON CONFLICT(session_address) DO UPDATE SET
                    highest_counter = excluded.highest_counter,
                    recent_tags_json = excluded.recent_tags_json,
                    updated_at_unix_ms = excluded.updated_at_unix_ms
                ",
                params![
                    replay_window.session_address.storage_key(),
                    replay_window.highest_counter as i64,
                    serde_json::to_string(&replay_window.recent_tags)
                        .map_err(map_serde_error_to_storage)?,
                    replay_window.updated_at_unix_ms,
                    profile_id.0,
                ],
            )
            .map_err(map_sqlite_error)?;

        Ok(())
    }

    pub fn get_replay_window(
        &self,
        session_address: &SessionAddress,
    ) -> OmniResult<Option<ReplayWindowState>> {
        let profile_id = self.resolve_transition_profile_id()?;
        self.connection
            .query_row(
                "
                SELECT highest_counter, recent_tags_json, updated_at_unix_ms
                FROM replay_windows
                WHERE session_address = ?1
                  AND profile_id = ?2
                ",
                params![session_address.storage_key(), profile_id.0],
                |row| {
                    let recent_tags: Vec<String> = serde_json::from_str(&row.get::<_, String>(1)?)
                        .map_err(map_serde_error_to_storage)
                        .map_err(to_sqlite_user_error)?;
                    Ok(ReplayWindowState {
                        session_address: session_address.clone(),
                        highest_counter: row.get::<_, i64>(0)? as u64,
                        recent_tags,
                        updated_at_unix_ms: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(map_sqlite_error)
    }

    pub fn ensure_replay_window(
        &self,
        session_address: &SessionAddress,
        now_unix_ms: i64,
    ) -> OmniResult<ReplayWindowState> {
        if let Some(existing) = self.get_replay_window(session_address)? {
            Ok(existing)
        } else {
            let replay_window = new_replay_window_state(session_address.clone(), now_unix_ms);
            self.store_replay_window(&replay_window)?;
            Ok(replay_window)
        }
    }

    #[allow(clippy::too_many_arguments)] // constructor/builder — args are the record's fields
    pub fn upsert_message_security_metadata(
        &self,
        message_id: &str,
        dialog_key: &str,
        contact_id: Option<&ContactId>,
        session_address: Option<&SessionAddress>,
        remote_account_id: Option<&AccountId>,
        remote_device_id: Option<&DeviceId>,
        encryption_state: &str,
        bootstrap_state: Option<SessionBootstrapState>,
        payload_kind: Option<&str>,
        created_at_unix_ms: i64,
        updated_at_unix_ms: i64,
    ) -> OmniResult<()> {
        let profile_id = self.resolve_transition_profile_id()?;
        self.connection
            .execute(
                "
                INSERT INTO message_security_metadata (
                    message_id,
                    dialog_key,
                    contact_id,
                    session_address,
                    remote_account_id,
                    remote_device_id,
                    encryption_state,
                    bootstrap_state,
                    payload_kind,
                    created_at_unix_ms,
                    updated_at_unix_ms,
                    profile_id
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                ON CONFLICT(message_id) DO UPDATE SET
                    dialog_key = excluded.dialog_key,
                    contact_id = excluded.contact_id,
                    session_address = excluded.session_address,
                    remote_account_id = excluded.remote_account_id,
                    remote_device_id = excluded.remote_device_id,
                    encryption_state = excluded.encryption_state,
                    bootstrap_state = excluded.bootstrap_state,
                    payload_kind = excluded.payload_kind,
                    updated_at_unix_ms = excluded.updated_at_unix_ms
                ",
                params![
                    message_id,
                    normalize_dialog_key(dialog_key),
                    contact_id.map(|value| value.0.clone()),
                    session_address.map(|value| value.storage_key()),
                    remote_account_id.map(|value| value.0.clone()),
                    remote_device_id.map(|value| value.0.clone()),
                    encryption_state,
                    bootstrap_state.map(|value| value.as_db_value().to_string()),
                    payload_kind,
                    created_at_unix_ms,
                    updated_at_unix_ms,
                    profile_id.0,
                ],
            )
            .map_err(map_sqlite_error)?;

        Ok(())
    }
}
