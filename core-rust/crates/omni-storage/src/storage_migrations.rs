/// The schema this build writes. A build meets a database stamped with a HIGHER one — written by
/// a newer Omni — and refuses it before touching anything: its old migrations would "repair" what
/// they do not know (an Oct 3 build re-created `group_ops`, and the next start dropped the MLS
/// groups' rows on it; docs/bugs/old-build-wiped-groups.md). Bump it with any migration a build
/// before it must not run over.
pub const SCHEMA_VERSION: u32 = 1;
const SCHEMA_VERSION_KEY: &str = "schema_version";

impl Storage {
    /// Refuse a database a newer build has written (see [`SCHEMA_VERSION`]).
    fn refuse_newer_schema(&self) -> OmniResult<()> {
        if !self.table_exists("app_meta")? {
            return Ok(());
        }
        let stored: Option<String> = self
            .connection
            .query_row(
                "SELECT value FROM app_meta WHERE key = ?1",
                rusqlite::params![SCHEMA_VERSION_KEY],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sqlite_error)?;
        match stored.and_then(|value| value.parse::<u32>().ok()) {
            Some(stored) if stored > SCHEMA_VERSION => Err(OmniError::Storage(format!(
                "this profile was opened by a newer Omni (database schema {stored}, this build knows {SCHEMA_VERSION}): update the app; nothing was changed"
            ))),
            _ => Ok(()),
        }
    }

    fn table_exists(&self, table_name: &str) -> OmniResult<bool> {
        let exists: i64 = self
            .connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                params![table_name],
                |row| row.get(0),
            )
            .map_err(map_sqlite_error)?;

        Ok(exists > 0)
    }

    pub fn migrate(&self) -> OmniResult<()> {
        self.refuse_newer_schema()?;
        let started_transaction = if self.connection.is_autocommit() {
            self.connection
                .execute_batch("BEGIN IMMEDIATE TRANSACTION")
                .map_err(|error| {
                    OmniError::Storage(format!(
                        "failed to begin migration transaction: {error}"
                    ))
                })?;
            true
        } else {
            false
        };

        let migration_result = (|| -> OmniResult<()> {
        self.execute_migration_batch(
            "create_base_schema",
            "
            CREATE TABLE IF NOT EXISTS messages (
                id TEXT PRIMARY KEY NOT NULL,
                body TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at_unix_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS app_meta (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            ",
        )?;

        self.ensure_messages_column("direction", "TEXT NOT NULL DEFAULT 'outgoing'")?;
        self.ensure_messages_column("updated_at_unix_ms", "INTEGER NOT NULL DEFAULT 0")?;
        self.ensure_messages_column("dialog_key", "TEXT")?;
        self.ensure_messages_column("remote_delivered_at_unix_ms", "INTEGER")?;
        self.connection
            .execute(
                "UPDATE messages SET updated_at_unix_ms = created_at_unix_ms WHERE updated_at_unix_ms = 0",
                [],
            )
            .map_err(map_sqlite_error)?;
        // A chat was once addressed by the peer's IP (`legacy-ip:`, the pre-mesh QUIC engine): the
        // address column and its index go (2026-10-14). Every row since has its dialog key.
        if self.messages_column_exists("peer_addr")? {
            self.execute_migration_batch(
                "messages_drop_column_peer_addr",
                "DROP INDEX IF EXISTS idx_messages_profile_peer_created; ALTER TABLE messages DROP COLUMN peer_addr;",
            )?;
        }
        self.connection
            .execute(
                "
                UPDATE messages
                SET status = 'sent'
                WHERE direction = 'outgoing'
                  AND status = 'delivered'
                  AND (remote_delivered_at_unix_ms IS NULL OR remote_delivered_at_unix_ms = 0)
                ",
                [],
            )
            .map_err(map_sqlite_error)?;

        self.execute_migration_batch(
            "create_stage3_foundation_tables",
            "
            CREATE TABLE IF NOT EXISTS outbound_queue (
                message_id TEXT PRIMARY KEY NOT NULL,
                dialog_key TEXT NOT NULL,
                status TEXT NOT NULL,
                attempt_count INTEGER NOT NULL DEFAULT 0,
                next_attempt_unix_ms INTEGER,
                last_attempt_unix_ms INTEGER,
                last_failure_kind TEXT,
                last_failure_message TEXT,
                remote_delivered_at_unix_ms INTEGER,
                created_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS inbound_dedup (
                message_id TEXT PRIMARY KEY NOT NULL,
                dialog_key TEXT NOT NULL,
                first_seen_unix_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS delivery_receipts (
                message_id TEXT PRIMARY KEY NOT NULL,
                dialog_key TEXT NOT NULL,
                delivered_at_unix_ms INTEGER NOT NULL,
                recorded_at_unix_ms INTEGER NOT NULL
            );
            ",
        )?;

        self.execute_migration_batch(
            "create_stage3_attachment_tables",
            "
            CREATE TABLE IF NOT EXISTS message_attachments (
                message_id TEXT PRIMARY KEY NOT NULL,
                dialog_key TEXT NOT NULL,
                file_name TEXT NOT NULL,
                file_size_bytes INTEGER NOT NULL,
                mime_type TEXT,
                content_id_hex TEXT NOT NULL,
                state TEXT NOT NULL,
                bytes_transferred INTEGER NOT NULL DEFAULT 0,
                local_blob_path TEXT,
                local_serving_enabled INTEGER NOT NULL DEFAULT 0,
                remote_holder_available INTEGER NOT NULL DEFAULT 0,
                local_saved_path TEXT,
                temp_download_path TEXT,
                last_error TEXT,
                created_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL,
                profile_id TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_message_attachments_dialog
            ON message_attachments(dialog_key, updated_at_unix_ms);

            CREATE INDEX IF NOT EXISTS idx_message_attachments_profile_dialog
            ON message_attachments(profile_id, dialog_key, updated_at_unix_ms);
            ",
        )?;
        // The file's identity is the root of its hash tree, not the SHA-256 of the whole file
        // (docs/decisions/attachment-content-tree.md). Rows written before keep the old value under
        // the new name: they still show and open, but a transfer of one is refused (no tree to check
        // against) — send it again.
        if self.table_column_exists("message_attachments", "sha256_hex")? {
            self.execute_migration_batch(
                "message_attachments_rename_sha256_to_content_id",
                "ALTER TABLE message_attachments RENAME COLUMN sha256_hex TO content_id_hex;",
            )?;
        }
        // The block layer of the tree (32 bytes per 16 MiB, concatenated): the sender's, and the
        // receiver's once its copy is complete — a copy is served on.
        self.ensure_table_column(
            "message_attachments",
            "content_tree_blocks",
            "BLOB",
        )?;
        // A download's map: one bit per 16 MiB block checked and on disk (blocks land in any
        // order, from several sources — attachment-content-tree.md, stage 3).
        self.ensure_table_column(
            "message_attachments",
            "content_blocks_have",
            "BLOB",
        )?;
        self.ensure_table_column(
            "message_attachments",
            "profile_id",
            "TEXT",
        )?;
        self.ensure_table_column(
            "message_attachments",
            "content_encryption",
            "TEXT",
        )?;
        self.ensure_table_column(
            "message_attachments",
            "content_key_hex",
            "TEXT",
        )?;
        self.ensure_table_column(
            "message_attachments",
            "content_nonce_seed_hex",
            "TEXT",
        )?;
        self.ensure_table_column(
            "message_attachments",
            "content_chunk_size_bytes",
            "INTEGER",
        )?;
        self.ensure_table_column(
            "message_attachments",
            "active_transfer_id",
            "TEXT",
        )?;
        self.ensure_table_column(
            "message_attachments",
            "local_blob_managed",
            "INTEGER NOT NULL DEFAULT 1",
        )?;
        self.ensure_table_column(
            "message_attachments",
            "local_source_size_bytes",
            "INTEGER",
        )?;
        self.ensure_table_column(
            "message_attachments",
            "local_source_modified_unix_ms",
            "INTEGER",
        )?;
        // How the chat shows the file (AttachmentPresentationV1 as JSON; NULL = a file plate).
        self.ensure_table_column(
            "message_attachments",
            "presentation_json",
            "TEXT",
        )?;
        self.connection
            .execute(
                "
                CREATE INDEX IF NOT EXISTS idx_message_attachments_profile_transfer
                ON message_attachments(profile_id, active_transfer_id)
                ",
                [],
            )
            .map_err(map_sqlite_error)?;

        self.connection
            .execute(
                "
                INSERT OR IGNORE INTO outbound_queue (
                    message_id,
                    dialog_key,
                    status,
                    attempt_count,
                    next_attempt_unix_ms,
                    last_attempt_unix_ms,
                    created_at_unix_ms,
                    updated_at_unix_ms
                )
                SELECT
                    id,
                    dialog_key,
                    status,
                    0,
                    NULL,
                    CASE
                        WHEN status = 'queued' THEN NULL
                        ELSE updated_at_unix_ms
                    END,
                    created_at_unix_ms,
                    updated_at_unix_ms
                FROM messages
                WHERE direction = 'outgoing'
                  AND status IN ('queued', 'sending', 'sent', 'retrying', 'failed')
                ",
                [],
            )
            .map_err(map_sqlite_error)?;

        self.execute_migration_batch(
            "create_stage4_identity_tables",
            "
            CREATE TABLE IF NOT EXISTS local_account (
                singleton INTEGER NOT NULL DEFAULT 1,
                account_id TEXT PRIMARY KEY NOT NULL,
                display_name TEXT NOT NULL,
                recovery_version INTEGER NOT NULL,
                recovery_code TEXT NOT NULL,
                restored_from_recovery INTEGER NOT NULL DEFAULT 0,
                created_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS local_devices (
                device_id TEXT PRIMARY KEY NOT NULL,
                account_id TEXT NOT NULL,
                device_label TEXT NOT NULL,
                is_current INTEGER NOT NULL DEFAULT 0,
                restored_from_recovery INTEGER NOT NULL DEFAULT 0,
                created_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL,
                FOREIGN KEY(account_id) REFERENCES local_account(account_id)
            );

            DROP INDEX IF EXISTS ux_local_devices_current;

            CREATE INDEX IF NOT EXISTS idx_local_devices_current
            ON local_devices(is_current)
            WHERE is_current = 1;
            ",
        )?;
        self.ensure_table_column(
            "local_account",
            "account_representation_version",
            "INTEGER NOT NULL DEFAULT 1",
        )?;
        self.ensure_table_column("local_account", "profile_id", "TEXT")?;
        self.ensure_table_column(
            "local_account",
            "profile_state",
            "TEXT NOT NULL DEFAULT 'signed_in'",
        )?;
        self.ensure_table_column(
            "local_account",
            "public_profile_updated_at_unix_ms",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        self.ensure_table_column("local_account", "public_avatar_hash_hex", "TEXT")?;
        self.ensure_table_column("local_account", "public_avatar_mime_type", "TEXT")?;

        self.execute_migration_batch(
            "create_stage4_contact_tables",
            "
            CREATE TABLE IF NOT EXISTS contacts (
                contact_id TEXT PRIMARY KEY NOT NULL,
                petname TEXT NOT NULL,
                remote_account_id TEXT,
                public_display_name TEXT,
                public_avatar_pending_hash_hex TEXT,
                public_avatar_pending_mime_type TEXT,
                remote_instance_id TEXT,
                relationship_state TEXT NOT NULL DEFAULT 'approved',
                pending_request_direction TEXT,
                is_blocked INTEGER NOT NULL DEFAULT 0,
                removed_at_unix_ms INTEGER,
                created_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS contact_dialog_links (
                contact_id TEXT NOT NULL,
                dialog_key TEXT NOT NULL,
                is_primary INTEGER NOT NULL DEFAULT 0,
                created_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY (contact_id, dialog_key),
                FOREIGN KEY(contact_id) REFERENCES contacts(contact_id)
            );

            CREATE UNIQUE INDEX IF NOT EXISTS ux_contact_dialog_links_primary
            ON contact_dialog_links(contact_id)
            WHERE is_primary = 1;

            CREATE TABLE IF NOT EXISTS contact_verifications (
                contact_id TEXT PRIMARY KEY NOT NULL,
                verified_at_unix_ms INTEGER NOT NULL,
                verification_method TEXT NOT NULL,
                verified_identity_key TEXT,
                updated_at_unix_ms INTEGER NOT NULL,
                FOREIGN KEY(contact_id) REFERENCES contacts(contact_id)
            );

            CREATE TABLE IF NOT EXISTS contact_identity_observations (
                contact_id TEXT PRIMARY KEY NOT NULL,
                identity_key TEXT NOT NULL,
                observed_display_name TEXT,
                observed_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL,
                FOREIGN KEY(contact_id) REFERENCES contacts(contact_id)
            );
            ",
        )?;
        self.ensure_table_column("contacts", "remote_account_id", "TEXT")?;
        self.ensure_table_column(
            "contacts",
            "public_avatar_hash_hex",
            "TEXT",
        )?;
        self.ensure_table_column(
            "contacts",
            "public_avatar_mime_type",
            "TEXT",
        )?;
        self.ensure_table_column(
            "contacts",
            "public_profile_updated_at_unix_ms",
            "INTEGER",
        )?;
        self.ensure_table_column(
            "contacts",
            "relationship_state",
            "TEXT NOT NULL DEFAULT 'approved'",
        )?;
        self.ensure_table_column("contacts", "pending_request_direction", "TEXT")?;

        self.execute_migration_batch(
            "create_stage5_crypto_tables",
            "
            CREATE TABLE IF NOT EXISTS identity_keys (
                account_id TEXT PRIMARY KEY NOT NULL,
                public_key TEXT NOT NULL,
                private_key_protection_scheme TEXT NOT NULL,
                private_key_blob TEXT NOT NULL,
                suite_version INTEGER NOT NULL,
                created_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL,
                FOREIGN KEY(account_id) REFERENCES local_account(account_id)
            );

            CREATE TABLE IF NOT EXISTS device_keys (
                device_id TEXT PRIMARY KEY NOT NULL,
                account_id TEXT NOT NULL,
                public_key TEXT NOT NULL,
                private_key_protection_scheme TEXT NOT NULL,
                private_key_blob TEXT NOT NULL,
                suite_version INTEGER NOT NULL,
                created_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL,
                FOREIGN KEY(device_id) REFERENCES local_devices(device_id),
                FOREIGN KEY(account_id) REFERENCES local_account(account_id)
            );

            CREATE TABLE IF NOT EXISTS signed_prekeys (
                device_id TEXT NOT NULL,
                prekey_id INTEGER NOT NULL,
                public_key TEXT NOT NULL,
                private_key_protection_scheme TEXT NOT NULL,
                private_key_blob TEXT NOT NULL,
                signature_r TEXT NOT NULL,
                signature_s TEXT NOT NULL,
                suite_version INTEGER NOT NULL,
                state TEXT NOT NULL DEFAULT 'active',
                created_at_unix_ms INTEGER NOT NULL,
                expires_at_unix_ms INTEGER,
                updated_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY (device_id, prekey_id),
                FOREIGN KEY(device_id) REFERENCES local_devices(device_id)
            );

            CREATE TABLE IF NOT EXISTS one_time_prekeys (
                device_id TEXT NOT NULL,
                prekey_id INTEGER NOT NULL,
                public_key TEXT NOT NULL,
                private_key_protection_scheme TEXT NOT NULL,
                private_key_blob TEXT NOT NULL,
                suite_version INTEGER NOT NULL,
                state TEXT NOT NULL DEFAULT 'available',
                created_at_unix_ms INTEGER NOT NULL,
                consumed_at_unix_ms INTEGER,
                updated_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY (device_id, prekey_id),
                FOREIGN KEY(device_id) REFERENCES local_devices(device_id)
            );

            CREATE INDEX IF NOT EXISTS idx_signed_prekeys_device_state
            ON signed_prekeys(device_id, state, created_at_unix_ms DESC, prekey_id DESC);

            CREATE INDEX IF NOT EXISTS idx_one_time_prekeys_device_state
            ON one_time_prekeys(device_id, state, created_at_unix_ms ASC, prekey_id ASC);

            CREATE TABLE IF NOT EXISTS sessions (
                session_address TEXT PRIMARY KEY NOT NULL,
                remote_account_id TEXT NOT NULL,
                remote_device_id TEXT NOT NULL,
                local_account_id TEXT NOT NULL,
                local_device_id TEXT NOT NULL,
                local_identity_key TEXT NOT NULL,
                local_device_key TEXT NOT NULL,
                remote_identity_key TEXT NOT NULL,
                remote_device_key TEXT NOT NULL,
                session_key_protection_scheme TEXT NOT NULL,
                session_key_blob TEXT NOT NULL,
                bootstrap_state TEXT NOT NULL,
                send_counter INTEGER NOT NULL DEFAULT 0,
                receive_counter INTEGER NOT NULL DEFAULT 0,
                remote_signed_prekey_id INTEGER NOT NULL,
                remote_one_time_prekey_id INTEGER,
                suite_version INTEGER NOT NULL,
                created_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS replay_windows (
                session_address TEXT PRIMARY KEY NOT NULL,
                highest_counter INTEGER NOT NULL DEFAULT 0,
                recent_tags_json TEXT NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS remote_device_observations (
                remote_device_id TEXT PRIMARY KEY NOT NULL,
                remote_account_id TEXT NOT NULL,
                device_public_key TEXT NOT NULL,
                device_label TEXT,
                observed_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS remote_prekey_bundles (
                bundle_key TEXT PRIMARY KEY NOT NULL,
                remote_account_id TEXT NOT NULL,
                remote_device_id TEXT NOT NULL,
                account_display_name TEXT,
                device_label TEXT,
                identity_public_key TEXT NOT NULL,
                device_public_key TEXT NOT NULL,
                signed_prekey_id INTEGER NOT NULL,
                signed_prekey_public_key TEXT NOT NULL,
                signed_prekey_signature_r TEXT NOT NULL,
                signed_prekey_signature_s TEXT NOT NULL,
                one_time_prekey_id INTEGER,
                one_time_prekey_public_key TEXT,
                generated_at_unix_ms INTEGER NOT NULL,
                observed_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL,
                version INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS message_security_metadata (
                message_id TEXT PRIMARY KEY NOT NULL,
                dialog_key TEXT NOT NULL,
                contact_id TEXT,
                session_address TEXT,
                remote_account_id TEXT,
                remote_device_id TEXT,
                encryption_state TEXT NOT NULL,
                bootstrap_state TEXT,
                payload_kind TEXT,
                created_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL,
                FOREIGN KEY(contact_id) REFERENCES contacts(contact_id)
            );
            ",
        )?;
        self.migrate_prekey_tables_to_device_scoped_primary_keys()?;

        // Direct-route hints fed the pre-mesh transport's dialer, which is gone; the table goes too
        // (docs/decisions/no-friend-tier.md, part 2). Idempotent, like every step here.
        self.execute_migration_batch(
            "drop_direct_route_hints",
            "DROP TABLE IF EXISTS direct_route_hints;",
        )?;
        // The excised multi-device sync left its tables behind (docs/decisions/
        // multi-device-sync-excision-phaseA.md); the new device model keeps its own
        // (docs/decisions/multi-device-equal-nodes.md).
        self.execute_migration_batch(
            "drop_excised_sync_tables",
            "
            DROP TABLE IF EXISTS device_roles;
            DROP TABLE IF EXISTS device_link_requests;
            DROP TABLE IF EXISTS device_link_offers;
            DROP TABLE IF EXISTS sync_checkpoints;
            DROP TABLE IF EXISTS sync_journal;
            DROP TABLE IF EXISTS device_revocation_markers;
            DROP TABLE IF EXISTS sync_diagnostics_metadata;
            DROP TABLE IF EXISTS synced_delivery_metadata;
            DROP TABLE IF EXISTS linked_devices;
            ",
        )?;
        // The account key chain (docs/decisions/multi-device-equal-nodes.md): the public master
        // key and the working key — portable, it travels with a copied folder — and the
        // certificates of the account's devices, this one included.
        self.execute_migration_batch(
            "create_account_key_tables",
            "
            CREATE TABLE IF NOT EXISTS account_keys (
                account_id TEXT PRIMARY KEY NOT NULL,
                master_public_key TEXT NOT NULL,
                working_key_version INTEGER NOT NULL,
                working_key_protection_scheme TEXT NOT NULL,
                working_key_blob TEXT NOT NULL,
                working_key_certificate_json TEXT NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS account_devices (
                account_id TEXT NOT NULL,
                device_id TEXT NOT NULL,
                certificate_chain_json TEXT NOT NULL,
                device_label TEXT NOT NULL,
                first_seen_at_unix_ms INTEGER NOT NULL,
                revoked_at_unix_ms INTEGER,
                PRIMARY KEY (account_id, device_id)
            );
            ",
        )?;
        // Which of the account's devices the owner has already been told about: a device that shows up
        // with no acknowledgement is "new" (multi-device-equal-nodes.md, «Устройства»). Rows that exist
        // when the column is added count as seen — nobody is told about devices they already had.
        if !self.table_column_exists("account_devices", "acknowledged_at_unix_ms")? {
            self.execute_migration_batch(
                "account_devices_add_acknowledged_at",
                "
                ALTER TABLE account_devices ADD COLUMN acknowledged_at_unix_ms INTEGER;
                UPDATE account_devices SET acknowledged_at_unix_ms = first_seen_at_unix_ms;
                ",
            )?;
        }
        // A revoked device the owner took out of the Settings list (maintainers, 2026-10-10: «удалить
        // какое-либо из истории отозванных»). Only the LIST forgets it: the row and its revocation
        // stay, so the device remains revoked and is still told so when it comes back.
        if !self.table_column_exists("account_devices", "hidden_at_unix_ms")? {
            self.execute_migration_batch(
                "account_devices_add_hidden_at",
                "
                ALTER TABLE account_devices ADD COLUMN hidden_at_unix_ms INTEGER;
                ",
            )?;
        }
        // The newest working key version seen for each account, and a working key handed to this
        // device while the profile password was not at hand (multi-device-equal-nodes.md, stage 3).
        self.execute_migration_batch(
            "create_account_working_keys",
            "
            CREATE TABLE IF NOT EXISTS account_working_keys (
                account_id TEXT PRIMARY KEY NOT NULL,
                version INTEGER NOT NULL,
                certificate_json TEXT NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS account_key_moves (
                account_id TEXT PRIMARY KEY NOT NULL,
                version INTEGER NOT NULL,
                move_json TEXT NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS pending_key_move_devices (
                account_id TEXT NOT NULL,
                device_id TEXT NOT NULL,
                version INTEGER NOT NULL,
                PRIMARY KEY (account_id, device_id)
            );
            CREATE TABLE IF NOT EXISTS pending_key_move_seeds (
                account_id TEXT PRIMARY KEY NOT NULL,
                version INTEGER NOT NULL,
                protection_scheme TEXT NOT NULL,
                blob_hex TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS pending_working_keys (
                account_id TEXT PRIMARY KEY NOT NULL,
                version INTEGER NOT NULL,
                certificate_json TEXT NOT NULL,
                protection_scheme TEXT NOT NULL,
                blob_hex TEXT NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL
            );
            ",
        )?;
        // Revoked devices: signed by the account's working key, kept as received so they can be
        // passed on (multi-device-equal-nodes.md, stage 3).
        self.execute_migration_batch(
            "create_device_revocations",
            "
            CREATE TABLE IF NOT EXISTS device_revocations (
                account_id TEXT NOT NULL,
                device_id TEXT NOT NULL,
                revocation_json TEXT NOT NULL,
                working_key_version INTEGER NOT NULL,
                revoked_at_unix_ms INTEGER NOT NULL,
                received_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY (account_id, device_id)
            );
            ",
        )?;
        // 'revoked' = a signed revocation; 'superseded' = the account moved to a newer working key and
        // this device was not re-certified (no statement to keep, `revocation_json` is empty).
        if !self.table_column_exists("device_revocations", "kind")? {
            self.execute_migration_batch(
                "device_revocations_add_kind",
                "ALTER TABLE device_revocations ADD COLUMN kind TEXT NOT NULL DEFAULT 'revoked';",
            )?;
        }
        // Our contacts as state shared by our devices (multi-device-equal-nodes.md, «Синхронизация
        // контактов»): one row per contact account, removals kept as tombstones; `seq` orders this
        // device's changes, and each of our devices acknowledges how far it has applied them.
        self.execute_migration_batch(
            "create_contact_sync",
            "
            CREATE TABLE IF NOT EXISTS contact_sync_records (
                profile_id TEXT NOT NULL,
                remote_account_id TEXT NOT NULL,
                state TEXT NOT NULL,
                state_version INTEGER NOT NULL,
                state_writer TEXT NOT NULL,
                petname TEXT NOT NULL,
                petname_version INTEGER NOT NULL,
                petname_writer TEXT NOT NULL,
                display_name TEXT,
                seq INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY (profile_id, remote_account_id)
            );
            CREATE TABLE IF NOT EXISTS contact_sync_device_marks (
                profile_id TEXT NOT NULL,
                device_id TEXT NOT NULL,
                acked_seq INTEGER NOT NULL DEFAULT 0,
                sent_through_seq INTEGER NOT NULL DEFAULT 0,
                sent_at_unix_ms INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (profile_id, device_id)
            );
            ",
        )?;
        // Where the devices we talk to post and look (courier-metadata-and-quota.md, step 6).
        self.execute_migration_batch(
            "create_peer_courier_sets",
            "
            CREATE TABLE IF NOT EXISTS peer_courier_sets (
                profile_id TEXT NOT NULL,
                remote_account_id TEXT NOT NULL,
                remote_device_id TEXT NOT NULL,
                publish_json TEXT NOT NULL,
                read_json TEXT NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY (profile_id, remote_account_id, remote_device_id)
            );
            ",
        )?;
        // The account passport: ours, its holders, and the ones we keep for others
        // (account-recovery-from-friends.md, Б1).
        self.execute_migration_batch(
            "create_account_passport_tables",
            "
            CREATE TABLE IF NOT EXISTS own_passport (
                profile_id TEXT PRIMARY KEY NOT NULL,
                version INTEGER NOT NULL,
                fingerprint TEXT NOT NULL,
                sealed BLOB NOT NULL,
                built_at_unix_ms INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS passport_holders (
                profile_id TEXT NOT NULL,
                account_id TEXT NOT NULL,
                chosen_at_unix_ms INTEGER NOT NULL,
                sent_version INTEGER NOT NULL DEFAULT 0,
                sent_at_unix_ms INTEGER NOT NULL DEFAULT 0,
                confirmed_version INTEGER NOT NULL DEFAULT 0,
                confirmed_at_unix_ms INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (profile_id, account_id)
            );
            CREATE TABLE IF NOT EXISTS held_passports (
                profile_id TEXT NOT NULL,
                owner_account_id TEXT NOT NULL,
                locator_hex TEXT NOT NULL,
                version INTEGER NOT NULL,
                sealed BLOB NOT NULL,
                mark_secret_hex TEXT NOT NULL,
                stored_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY (profile_id, owner_account_id)
            );
            CREATE TABLE IF NOT EXISTS recovered_groups (
                profile_id TEXT NOT NULL,
                group_id TEXT NOT NULL,
                name TEXT NOT NULL,
                role TEXT NOT NULL,
                recovered_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY (profile_id, group_id)
            );
            ",
        )?;
        // History sync: lines a contact's copy handed over, and how a restored chat compared with
        // its passport fingerprint (account-recovery-from-friends.md, Б2).
        self.execute_migration_batch(
            "create_history_sync_tables",
            "
            CREATE TABLE IF NOT EXISTS history_imports (
                profile_id TEXT NOT NULL,
                message_id TEXT NOT NULL,
                source_account_id TEXT NOT NULL,
                imported_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY (profile_id, message_id)
            );
            CREATE TABLE IF NOT EXISTS chat_history_checks (
                profile_id TEXT NOT NULL,
                chat_account_id TEXT NOT NULL,
                state TEXT NOT NULL,
                checked_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY (profile_id, chat_account_id)
            );
            ",
        )?;
        // Group lines as their authors signed them: a newcomer's history (group-history-sync.md).
        self.execute_migration_batch(
            "create_group_signed_lines",
            "
            CREATE TABLE IF NOT EXISTS group_signed_lines (
                profile_id TEXT NOT NULL,
                group_id TEXT NOT NULL,
                message_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                sender_pub TEXT NOT NULL,
                sender_name TEXT NOT NULL,
                body TEXT NOT NULL,
                authored_at_unix_ms INTEGER NOT NULL,
                signature_b64 TEXT NOT NULL,
                PRIMARY KEY (profile_id, message_id)
            );
            CREATE INDEX IF NOT EXISTS group_signed_lines_by_group
                ON group_signed_lines (profile_id, group_id, authored_at_unix_ms);
            ",
        )?;
        // A group that went on in two branches, and members asking to be put back
        // (group-history-sync.md, «Синхронизация эпох»).
        self.execute_migration_batch(
            "create_group_branches",
            "
            CREATE TABLE IF NOT EXISTS group_branches (
                profile_id TEXT NOT NULL,
                group_id TEXT NOT NULL,
                fork_epoch INTEGER NOT NULL,
                ours_wins INTEGER NOT NULL,
                first_seen_at_unix_ms INTEGER NOT NULL,
                rejoining_since_unix_ms INTEGER,
                requested_at_unix_ms INTEGER NOT NULL DEFAULT 0,
                request_mls BLOB,
                PRIMARY KEY (profile_id, group_id)
            );
            CREATE TABLE IF NOT EXISTS group_pending_welcomes (
                profile_id TEXT NOT NULL,
                group_id TEXT NOT NULL,
                member_pub TEXT NOT NULL,
                epoch_before INTEGER NOT NULL,
                commit_rank BLOB NOT NULL,
                welcome BLOB NOT NULL,
                created_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY (profile_id, group_id, member_pub)
            );
            CREATE TABLE IF NOT EXISTS group_readd_requests (
                profile_id TEXT NOT NULL,
                group_id TEXT NOT NULL,
                member_pub TEXT NOT NULL,
                key_package BLOB NOT NULL,
                seen_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY (profile_id, group_id, member_pub)
            );
            ",
        )?;
        // Which devices of a contact confirmed a message (multi-device-equal-nodes.md, 2.3).
        self.execute_migration_batch(
            "create_message_device_receipts",
            "
            CREATE TABLE IF NOT EXISTS message_device_receipts (
                message_id TEXT NOT NULL,
                remote_device_id TEXT NOT NULL,
                delivered_at_unix_ms INTEGER NOT NULL,
                profile_id TEXT NOT NULL,
                PRIMARY KEY (message_id, remote_device_id)
            );
            ",
        )?;
        self.execute_migration_batch(
            "create_stage10_profile_foundation_tables",
            "
            CREATE TABLE IF NOT EXISTS profile_registry (
                profile_id TEXT PRIMARY KEY NOT NULL,
                account_id TEXT NOT NULL UNIQUE,
                display_name TEXT NOT NULL,
                public_profile_updated_at_unix_ms INTEGER NOT NULL DEFAULT 0,
                public_avatar_hash_hex TEXT,
                public_avatar_mime_type TEXT,
                account_representation_version INTEGER NOT NULL DEFAULT 2,
                profile_state TEXT NOT NULL,
                restored_from_recovery INTEGER NOT NULL DEFAULT 0,
                created_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL,
                FOREIGN KEY(account_id) REFERENCES local_account(account_id)
            );

            CREATE TABLE IF NOT EXISTS profile_secrets (
                profile_id TEXT PRIMARY KEY NOT NULL,
                account_id TEXT NOT NULL UNIQUE,
                recovery_version INTEGER NOT NULL,
                recovery_code TEXT NOT NULL,
                password_enabled INTEGER NOT NULL DEFAULT 0,
                password_salt_hex TEXT,
                password_verifier_hex TEXT,
                profile_master_key_protection_scheme TEXT,
                profile_master_key_blob_hex TEXT,
                encrypted_profile_secrets_json TEXT,
                restored_from_recovery INTEGER NOT NULL DEFAULT 0,
                created_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL,
                FOREIGN KEY(profile_id) REFERENCES profile_registry(profile_id),
                FOREIGN KEY(account_id) REFERENCES local_account(account_id)
            );

            CREATE TABLE IF NOT EXISTS profile_current_device (
                profile_id TEXT PRIMARY KEY NOT NULL,
                device_id TEXT NOT NULL,
                created_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL,
                FOREIGN KEY(profile_id) REFERENCES profile_registry(profile_id),
                FOREIGN KEY(device_id) REFERENCES local_devices(device_id)
            );
            ",
        )?;
        self.execute_migration_batch(
            "create_stage11_contact_presence_tables",
            "
            CREATE TABLE IF NOT EXISTS contact_presence (
                contact_id TEXT NOT NULL,
                profile_id TEXT NOT NULL,
                last_seen_at_unix_ms INTEGER,
                last_sent_at_unix_ms INTEGER,
                created_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY (contact_id, profile_id)
            );

            CREATE INDEX IF NOT EXISTS idx_contact_presence_profile_updated
            ON contact_presence(profile_id, updated_at_unix_ms DESC);
            ",
        )?;
        self.ensure_table_column("messages", "profile_id", "TEXT")?;
        self.ensure_table_column(
            "profile_registry",
            "public_profile_updated_at_unix_ms",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        self.ensure_table_column("profile_registry", "public_avatar_hash_hex", "TEXT")?;
        self.ensure_table_column("profile_registry", "public_avatar_mime_type", "TEXT")?;
        self.ensure_table_column("outbound_queue", "profile_id", "TEXT")?;
        self.ensure_table_column("inbound_dedup", "profile_id", "TEXT")?;
        self.ensure_table_column("delivery_receipts", "profile_id", "TEXT")?;
        self.ensure_table_column("contacts", "profile_id", "TEXT")?;
        self.ensure_table_column("contact_dialog_links", "profile_id", "TEXT")?;
        self.ensure_table_column("contact_verifications", "profile_id", "TEXT")?;
        self.ensure_table_column("contact_identity_observations", "profile_id", "TEXT")?;
        // The safety number is of the account alone (2026-10-10, docs/bugs/
        // safety-number-differs-per-own-device.md): what a verification confirmed is the contact's
        // account id. One taken on the old inputs (a device set, or the crypto identity key) still
        // confirmed that contact, so it is kept, moved onto the account id; the device-set columns
        // and the observed devices go.
        if self.table_column_exists("contact_verifications", "verified_device_set_hash")? {
            self.execute_migration_batch(
                "contact_verifications_on_the_account_id",
                "
                UPDATE contact_verifications
                SET verified_identity_key = (
                    SELECT c.remote_account_id FROM contacts c
                    WHERE c.contact_id = contact_verifications.contact_id
                )
                WHERE EXISTS (
                    SELECT 1 FROM contacts c
                    WHERE c.contact_id = contact_verifications.contact_id
                      AND c.remote_account_id IS NOT NULL
                );
                ",
            )?;
            for column in [
                "verified_device_set_hash",
                "verified_device_count",
                "verified_fingerprint",
                "verified_safety_number",
            ] {
                self.execute_migration_batch(
                    &format!("contact_verifications_drop_column_{column}"),
                    &format!("ALTER TABLE contact_verifications DROP COLUMN {column};"),
                )?;
            }
        }
        self.execute_migration_batch(
            "drop_contact_device_observations",
            "DROP TABLE IF EXISTS contact_device_observations;",
        )?;
        self.ensure_table_column("contacts", "public_avatar_pending_hash_hex", "TEXT")?;
        self.ensure_table_column("contacts", "public_avatar_pending_mime_type", "TEXT")?;
        // "Friend" as a connectivity tier is gone (2026-10-04): the mesh keeps EVERY approved contact
        // linked, so the flag only steered a pre-mesh probe that did nothing the mesh doesn't. The
        // column goes with it. See docs/decisions/no-friend-tier.md.
        // The password guards the working key only (2026-10-07): the profile master key under the
        // password let a copied folder plus the password open the root secret, which must stay sealed
        // to its machine (docs/decisions/multi-device-equal-nodes.md). The `local_account` copies of
        // the profile secrets were written empty and never read since `profile_secrets` took them.
        if self.table_column_exists("profile_secrets", "password_wrapped_profile_master_key_json")? {
            self.execute_migration_batch(
                "profile_secrets_drop_password_wrapped_master_key",
                "ALTER TABLE profile_secrets DROP COLUMN password_wrapped_profile_master_key_json;",
            )?;
        }
        for column in [
            "password_enabled",
            "password_salt_hex",
            "password_verifier_hex",
            "password_wrapped_profile_master_key_json",
            "profile_master_key_protection_scheme",
            "profile_master_key_blob_hex",
            "encrypted_profile_secrets_json",
        ] {
            if self.table_column_exists("local_account", column)? {
                self.execute_migration_batch(
                    &format!("local_account_drop_column_{column}"),
                    &format!("ALTER TABLE local_account DROP COLUMN {column};"),
                )?;
            }
        }
        if self.table_column_exists("contacts", "is_friend")? {
            self.execute_migration_batch(
                "contacts_drop_column_is_friend",
                "ALTER TABLE contacts DROP COLUMN is_friend;",
            )?;
        }
        self.ensure_table_column("contacts", "has_met", "INTEGER NOT NULL DEFAULT 0")?;
        // Friend-proxy (Axis 2): per-contact consent to carry their mesh traffic through us.
        // Off by default, revocable. See docs/decisions/friend-proxy-by-consent.md (Phase C).
        self.ensure_table_column(
            "contacts",
            "friend_relay_consent",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        self.ensure_table_column(
            "contact_presence",
            "presence_state",
            "TEXT NOT NULL DEFAULT 'offline'",
        )?;
        self.ensure_table_column(
            "contact_presence",
            "last_activity_at_unix_ms",
            "INTEGER",
        )?;
        self.ensure_table_column(
            "contact_presence",
            "consecutive_missed_heartbeats",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        self.ensure_table_column(
            "contact_presence",
            "next_heartbeat_due_at_unix_ms",
            "INTEGER",
        )?;
        self.ensure_table_column("sessions", "profile_id", "TEXT")?;
        self.ensure_table_column("replay_windows", "profile_id", "TEXT")?;
        self.ensure_table_column("message_security_metadata", "profile_id", "TEXT")?;

        self.connection
            .execute(
                "
                UPDATE messages
                SET profile_id = COALESCE(
                    NULLIF(profile_id, ''),
                    (SELECT profile_id FROM local_account WHERE singleton = 1 LIMIT 1),
                    (SELECT pr.profile_id FROM profile_registry pr ORDER BY pr.created_at_unix_ms ASC LIMIT 1)
                )
                WHERE profile_id IS NULL OR profile_id = ''
                ",
                [],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                UPDATE outbound_queue
                SET profile_id = COALESCE(
                    NULLIF(profile_id, ''),
                    (
                        SELECT m.profile_id
                        FROM messages m
                        WHERE m.id = outbound_queue.message_id
                        LIMIT 1
                    ),
                    (SELECT profile_id FROM local_account WHERE singleton = 1 LIMIT 1),
                    (SELECT pr.profile_id FROM profile_registry pr ORDER BY pr.created_at_unix_ms ASC LIMIT 1)
                )
                WHERE profile_id IS NULL OR profile_id = ''
                ",
                [],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                UPDATE inbound_dedup
                SET profile_id = COALESCE(
                    NULLIF(profile_id, ''),
                    (SELECT profile_id FROM local_account WHERE singleton = 1 LIMIT 1),
                    (SELECT pr.profile_id FROM profile_registry pr ORDER BY pr.created_at_unix_ms ASC LIMIT 1)
                )
                WHERE profile_id IS NULL OR profile_id = ''
                ",
                [],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                UPDATE delivery_receipts
                SET profile_id = COALESCE(
                    NULLIF(profile_id, ''),
                    (
                        SELECT m.profile_id
                        FROM messages m
                        WHERE m.id = delivery_receipts.message_id
                        LIMIT 1
                    ),
                    (SELECT profile_id FROM local_account WHERE singleton = 1 LIMIT 1),
                    (SELECT pr.profile_id FROM profile_registry pr ORDER BY pr.created_at_unix_ms ASC LIMIT 1)
                )
                WHERE profile_id IS NULL OR profile_id = ''
                ",
                [],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                UPDATE message_attachments
                SET profile_id = COALESCE(
                    NULLIF(profile_id, ''),
                    (
                        SELECT m.profile_id
                        FROM messages m
                        WHERE m.id = message_attachments.message_id
                        LIMIT 1
                    ),
                    (SELECT profile_id FROM local_account WHERE singleton = 1 LIMIT 1),
                    (SELECT pr.profile_id FROM profile_registry pr ORDER BY pr.created_at_unix_ms ASC LIMIT 1)
                )
                WHERE profile_id IS NULL OR profile_id = ''
                ",
                [],
            )
            .map_err(map_sqlite_error)?;


        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_messages_profile_dialog_created ON messages(profile_id, dialog_key, created_at_unix_ms, id)",
                [],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_outbound_queue_profile_created ON outbound_queue(profile_id, created_at_unix_ms, message_id)",
                [],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_inbound_dedup_profile_dialog ON inbound_dedup(profile_id, dialog_key, first_seen_unix_ms)",
                [],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_delivery_receipts_profile_dialog ON delivery_receipts(profile_id, dialog_key, recorded_at_unix_ms)",
                [],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_contacts_profile_state ON contacts(profile_id, removed_at_unix_ms, petname, contact_id)",
                [],
            )
            .map_err(map_sqlite_error)?;
        // The presence tick (4 times a second) looks only at contacts not yet offline: with this
        // partial index it reads those few rows instead of walking every contact.
        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_contact_presence_not_offline ON contact_presence(profile_id, last_activity_at_unix_ms) WHERE presence_state <> 'offline'",
                [],
            )
            .map_err(map_sqlite_error)?;
        // A received frame finds its contact by account and by dialog: both lookups walked every
        // contact (every dialog link) without these.
        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_contacts_account_profile ON contacts(remote_account_id, profile_id)",
                [],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_contact_dialog_links_dialog ON contact_dialog_links(dialog_key, profile_id)",
                [],
            )
            .map_err(map_sqlite_error)?;
        // A contact's devices are looked up by account (every autoconnect tick, for every contact):
        // without this each lookup walked the whole table.
        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_remote_prekey_bundles_account ON remote_prekey_bundles(remote_account_id, observed_at_unix_ms DESC)",
                [],
            )
            .map_err(map_sqlite_error)?;
        // Same for downloads waiting to be requested (every tick): only those rows, not every
        // attachment the profile ever had.
        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_message_attachments_pending_download ON message_attachments(profile_id, dialog_key, updated_at_unix_ms) WHERE remote_holder_available = 1 AND state IN ('requested', 'waiting_for_reconnect')",
                [],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_contact_dialog_links_profile_contact ON contact_dialog_links(profile_id, contact_id, is_primary, updated_at_unix_ms DESC)",
                [],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_contact_verifications_profile_updated ON contact_verifications(profile_id, updated_at_unix_ms DESC)",
                [],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_contact_identity_observations_profile_updated ON contact_identity_observations(profile_id, updated_at_unix_ms DESC)",
                [],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_sessions_profile_remote_account ON sessions(profile_id, remote_account_id, updated_at_unix_ms DESC)",
                [],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_replay_windows_profile_session ON replay_windows(profile_id, session_address)",
                [],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_message_security_metadata_profile_dialog ON message_security_metadata(profile_id, dialog_key, created_at_unix_ms DESC)",
                [],
            )
            .map_err(map_sqlite_error)?;

        self.ensure_table_column("identity_keys", "signing_public_key", "TEXT")?;
        // Where the device's identity lives (docs/decisions/multi-device-equal-nodes.md, 1.4).
        self.ensure_table_column("local_devices", "bound_db_path", "TEXT")?;
        self.ensure_table_column("remote_prekey_bundles", "device_certificate_chain_json", "TEXT")?;
        // Which device of the contact answers on a dialog (multi-device-equal-nodes.md, 2.3).
        self.ensure_table_column("contact_dialog_links", "remote_device_id", "TEXT")?;
        // Our other devices as peers, and the messages they wrote (multi-device-equal-nodes.md, 2.4).
        self.ensure_table_column("account_devices", "dialog_key", "TEXT")?;
        // When we last heard an own device (a link to it, its fresh beacon): a restart keeps its
        // silence, so a long-gone copy is not looked for at full cadence again (2026-10-15).
        self.ensure_table_column("account_devices", "last_heard_at_unix_ms", "INTEGER")?;
        // The account's own rendezvous topic secret, under this machine's protection: readable
        // without the profile password, so own devices keep finding each other while locked.
        self.ensure_table_column("account_keys", "rendezvous_secret_protection_scheme", "TEXT")?;
        self.ensure_table_column("account_keys", "rendezvous_secret_blob", "TEXT")?;
        self.ensure_messages_column("copied_from_device_id", "TEXT")?;
        self.ensure_table_column(
            "identity_keys",
            "signing_private_key_protection_scheme",
            "TEXT",
        )?;
        self.ensure_table_column("identity_keys", "signing_private_key_blob", "TEXT")?;
        // Nothing reads the per-account crypto identity any more: the safety number is of the
        // account id, each device vouches for its keys by its certificate (2026-10-10).
        self.execute_migration_batch(
            "drop_remote_identity_observations",
            "DROP TABLE IF EXISTS remote_identity_observations;",
        )?;
        self.ensure_table_column(
            "remote_prekey_bundles",
            "identity_signing_public_key",
            "TEXT",
        )?;
        self.ensure_table_column(
            "sessions",
            "local_identity_signing_key",
            "TEXT",
        )?;
        self.ensure_table_column(
            "sessions",
            "remote_identity_signing_key",
            "TEXT",
        )?;
        self.ensure_table_column(
            "sessions",
            "send_chain_key_protection_scheme",
            "TEXT",
        )?;
        self.ensure_table_column("sessions", "send_chain_key_blob", "TEXT")?;
        self.ensure_table_column(
            "sessions",
            "receive_chain_key_protection_scheme",
            "TEXT",
        )?;
        self.ensure_table_column("sessions", "receive_chain_key_blob", "TEXT")?;
        self.ensure_table_column(
            "sessions",
            "local_ratchet_key_protection_scheme",
            "TEXT",
        )?;
        self.ensure_table_column("sessions", "local_ratchet_key_blob", "TEXT")?;
        self.ensure_table_column("sessions", "local_ratchet_public_key", "TEXT")?;
        self.ensure_table_column("sessions", "remote_ratchet_public_key", "TEXT")?;
        self.ensure_table_column("sessions", "previous_send_chain_length", "INTEGER NOT NULL DEFAULT 0")?;
        self.ensure_table_column("sessions", "needs_send_ratchet", "INTEGER NOT NULL DEFAULT 0")?;
        // Stable per-contact rendezvous secret (Option<ProtectedSecret>, two columns like
        // the chain keys). Seeds the blinded serverless-rendezvous topic + beacon AEAD;
        // NULL on sessions bootstrapped before it existed. See
        // docs/decisions/serverless-bootstrap-rendezvous.md §Integration.
        self.ensure_table_column("sessions", "rendezvous_secret_protection_scheme", "TEXT")?;
        self.ensure_table_column("sessions", "rendezvous_secret_blob", "TEXT")?;
        // Optimistic-concurrency revision of a session (`store_session_record`). Stored rows start
        // at 1 so a never-stored copy (revision 0) can only insert, never overwrite; rows written
        // before the column existed are lifted to 1 for the same reason. Idempotent.
        self.ensure_table_column("sessions", "revision", "INTEGER NOT NULL DEFAULT 0")?;
        // Keys of frames a later frame overtook (`SessionRecord::protected_skipped_message_keys`),
        // two columns like the chain keys; NULL = nothing remembered. Idempotent.
        self.ensure_table_column("sessions", "skipped_message_keys_protection_scheme", "TEXT")?;
        self.ensure_table_column("sessions", "skipped_message_keys_blob", "TEXT")?;
        self.execute_migration_batch(
            "sessions_revision_starts_at_one",
            "UPDATE sessions SET revision = 1 WHERE revision < 1;",
        )?;
        self.execute_migration_batch(
            "create_stage11_call_tables",
            "
            CREATE TABLE IF NOT EXISTS call_sessions (
                profile_id TEXT NOT NULL,
                call_id TEXT PRIMARY KEY NOT NULL,
                dialog_key TEXT NOT NULL,
                contact_id TEXT,
                direction TEXT NOT NULL,
                state TEXT NOT NULL,
                outcome TEXT,
                decline_reason TEXT,
                local_participant_id TEXT,
                remote_participant_id TEXT,
                initiator_account_id TEXT,
                initiator_device_id TEXT,
                responder_account_id TEXT,
                responder_device_id TEXT,
                local_mic_muted INTEGER NOT NULL DEFAULT 0,
                network_quality TEXT,
                started_at_unix_ms INTEGER NOT NULL,
                ringing_started_at_unix_ms INTEGER,
                connected_at_unix_ms INTEGER,
                ended_at_unix_ms INTEGER,
                reconnect_deadline_unix_ms INTEGER,
                updated_at_unix_ms INTEGER NOT NULL,
                FOREIGN KEY(contact_id) REFERENCES contacts(contact_id)
            );

            CREATE TABLE IF NOT EXISTS call_participants (
                call_id TEXT NOT NULL,
                participant_id TEXT NOT NULL,
                account_id TEXT NOT NULL,
                device_id TEXT NOT NULL,
                display_name TEXT,
                is_local INTEGER NOT NULL DEFAULT 0,
                is_muted INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(call_id, participant_id),
                FOREIGN KEY(call_id) REFERENCES call_sessions(call_id) ON DELETE CASCADE
            );
            ",
        )?;
        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_call_sessions_profile_dialog_updated ON call_sessions(profile_id, dialog_key, updated_at_unix_ms DESC, call_id DESC)",
                [],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_call_sessions_profile_state_updated ON call_sessions(profile_id, state, updated_at_unix_ms DESC, call_id DESC)",
                [],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_call_participants_call_local ON call_participants(call_id, is_local DESC, participant_id ASC)",
                [],
            )
            .map_err(map_sqlite_error)?;

        self.connection
            .execute(
                "DELETE FROM identity_keys WHERE suite_version < ?1",
                params![CRYPTO_SUITE_VERSION_CURRENT as i64],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "DELETE FROM device_keys WHERE suite_version < ?1",
                params![CRYPTO_SUITE_VERSION_CURRENT as i64],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "DELETE FROM signed_prekeys WHERE suite_version < ?1",
                params![CRYPTO_SUITE_VERSION_CURRENT as i64],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "DELETE FROM one_time_prekeys WHERE suite_version < ?1",
                params![CRYPTO_SUITE_VERSION_CURRENT as i64],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "DELETE FROM sessions WHERE suite_version < ?1",
                params![CRYPTO_SUITE_VERSION_CURRENT as i64],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                DELETE FROM replay_windows
                WHERE session_address NOT IN (SELECT session_address FROM sessions)
                ",
                [],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                DELETE FROM remote_prekey_bundles
                WHERE identity_signing_public_key IS NULL
                   OR length(identity_public_key) < 64
                   OR length(identity_signing_public_key) < 64
                   OR length(device_public_key) < 64
                   OR length(signed_prekey_public_key) < 64
                ",
                [],
            )
            .map_err(map_sqlite_error)?;

        self.execute_migration_batch(
            "create_durable_frames",
            "
            CREATE TABLE IF NOT EXISTS durable_frames (
                feed_id BLOB NOT NULL,
                seq INTEGER NOT NULL,
                wire BLOB NOT NULL,
                PRIMARY KEY (feed_id, seq)
            );

            CREATE TABLE IF NOT EXISTS durable_feed_seq (
                peer BLOB PRIMARY KEY NOT NULL,
                seq INTEGER NOT NULL
            );
            ",
        )?;

        // Persisted twin of the in-memory MeshDialogMap.by_node: which dialog a
        // mesh NodeAddress belongs to. Recorded at pairing/link-up while both are
        // known; lets a durable (store-and-forward) frame from a peer we never
        // linked *this session* resolve its dialog_key → contact → ratchet for
        // decrypt. See docs/decisions/mesh-durable-delivery.md.
        self.execute_migration_batch(
            "create_mesh_dialog_route",
            "
            CREATE TABLE IF NOT EXISTS mesh_dialog_route (
                node_address BLOB PRIMARY KEY NOT NULL,
                dialog_key TEXT NOT NULL
            );
            ",
        )?;

        // Persisted last-good DIRECT endpoints per mesh NodeAddress (warm rejoin / G3): on start the
        // app re-dials these cached endpoints immediately instead of re-running LAN/UPnP/rendezvous
        // cold. Endpoints are HINTS only — the ed25519 challenge on every dial is the sole authority,
        // so a stale entry just wastes a dial. TTL'd; refreshed by a live snapshot at runtime. See
        // docs/decisions/omni-network-model.md (D7).
        self.execute_migration_batch(
            "create_mesh_peer_book",
            "
            CREATE TABLE IF NOT EXISTS mesh_peer_book (
                node_address BLOB NOT NULL,
                dial_addr TEXT NOT NULL,
                observed_at_unix_ms INTEGER NOT NULL,
                expires_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY (node_address, dial_addr)
            );
            ",
        )?;

        // User-managed registry of network ENTRY nodes (bootstrap / relay) — the persistent list the
        // mesh dials when LAN + invite don't reach. The mechanism supports exactly one removable "base"
        // node seeded by the app layer, but NONE is seeded right now (the project VPS expired — see below);
        // the user adds/removes nodes from the GUI. The empty list MUST still connect (LAN + invite +
        // rendezvous). Endpoints are HINTS (the ed25519 challenge is the authority).
        // See docs/decisions/community-volunteer-nodes.md + docs/notes/roadmap-CONTINUE.md (Always-connect).
        self.execute_migration_batch(
            "create_network_nodes",
            "
            CREATE TABLE IF NOT EXISTS network_nodes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                endpoint TEXT,
                node_address BLOB,
                label TEXT NOT NULL,
                source TEXT NOT NULL,
                added_at_unix_ms INTEGER NOT NULL,
                tcp_endpoint TEXT,
                cert_spki_pin BLOB,
                bundle_id TEXT,
                bundle_version INTEGER,
                shared INTEGER NOT NULL DEFAULT 0
            );
            ",
        )?;

        // NO base bootstrap node is seeded right now: the project relay VPS (198.51.100.213) EXPIRED
        // (dead — every fresh profile logged `relay TCP carrier bind failed → UDP-only` and wasted a
        // dial timeout). The base-node MECHANISM is intact (this table, `source = 'base'`, GUI add/remove,
        // the seed structure) — when a live relay lands, re-add a seed batch here pointing at it (endpoint
        // + pinned identity + tcp_endpoint), mirroring what was removed 2026-07-24. "Works with an empty
        // bootstrap list" (LAN + invite + rendezvous) stays true, so a base-less fresh profile still
        // connects. This DELETE cleans the dead node out of EXISTING profiles (idempotent, guarded to the
        // exact expired endpoint so a future live base node is never touched; a user removal already
        // left no row → no-op). See docs/notes/roadmap-CONTINUE.md (Always-connect epic).
        self.execute_migration_batch(
            "remove_expired_base_network_node",
            "
            DELETE FROM network_nodes
            WHERE source = 'base'
              AND endpoint = '198.51.100.213:443';
            ",
        )?;

        // R1a connection-quality ladder: a relay carries the SEPARATE TLS-over-TCP bridge address
        // (`tcp_endpoint`, rung 0 — the rock-solid carrier that survives a UDP-hostile path like a UDP-dropping carrier;
        // distinct from the UDP/QUIC `endpoint` = rung 1) plus an optional `cert_spki_pin` (the SPKI
        // hash that pins the outer TLS for the R2 active-probe-grade fronting). Now part of the CREATE
        // above (fresh DBs); these guarded ALTERs BACKFILL a DB created before the columns existed
        // (the batches re-run on every open, and ALTER ADD COLUMN is not idempotent → column-exists
        // guard). Additive + nullable — a node may offer only UDP, only TCP, or both. See
        // docs/decisions/connection-quality-ladder.md.
        if !self.table_column_exists("network_nodes", "tcp_endpoint")? {
            self.execute_migration_batch(
                "network_nodes_add_tcp_endpoint",
                "ALTER TABLE network_nodes ADD COLUMN tcp_endpoint TEXT;",
            )?;
        }
        if !self.table_column_exists("network_nodes", "cert_spki_pin")? {
            self.execute_migration_batch(
                "network_nodes_add_cert_spki_pin",
                "ALTER TABLE network_nodes ADD COLUMN cert_spki_pin BLOB;",
            )?;
        }

        // R1c rotatable white-relay: relays imported from a signed `om3relay:` bundle are tagged with
        // their `bundle_id` (the issuing identity's bundle) + `bundle_version`, so a newer version
        // REPLACES the same bundle_id's rows (rotation) — a manually-added relay leaves both NULL.
        // In CREATE above (fresh DBs); these guarded ALTERs backfill older DBs. Additive + nullable.
        // See docs/decisions/r1c-rotatable-white-relay.md.
        if !self.table_column_exists("network_nodes", "bundle_id")? {
            self.execute_migration_batch(
                "network_nodes_add_bundle_id",
                "ALTER TABLE network_nodes ADD COLUMN bundle_id TEXT;",
            )?;
        }
        if !self.table_column_exists("network_nodes", "bundle_version")? {
            self.execute_migration_batch(
                "network_nodes_add_bundle_version",
                "ALTER TABLE network_nodes ADD COLUMN bundle_version INTEGER;",
            )?;
        }

        // R1c increment 3b: `shared` flags the rows I publish as MY OWN relay bundle to approved
        // friends (the issuer side of beacon auto-propagation). A per-row boolean — distinct from
        // `bundle_id` (which tags relays IMPORTED from a friend's bundle) so the two never conflate.
        // In CREATE above (fresh DBs); this guarded ALTER backfills older DBs. Additive, defaults 0.
        // See docs/decisions/r1c-rotatable-white-relay.md.
        // Remote-control sessions (docs/decisions/remote-access-for-contacts.md): the session's side
        // of a call (role + phase) as JSON; NULL for an ordinary call. Also what marks the chat line
        // «Сеанс управления».
        if !self.table_column_exists("call_sessions", "remote_control_json")? {
            self.execute_migration_batch(
                "call_sessions_add_remote_control_json",
                "ALTER TABLE call_sessions ADD COLUMN remote_control_json TEXT;",
            )?;
        }

        if !self.table_column_exists("network_nodes", "shared")? {
            self.execute_migration_batch(
                "network_nodes_add_shared",
                "ALTER TABLE network_nodes ADD COLUMN shared INTEGER NOT NULL DEFAULT 0;",
            )?;
        }

        // (The base node's TCP-carrier endpoint seed was removed with the expired VPS above — re-add it
        // alongside the base-node re-seed when a live relay lands.)

        // Rendezvous courier providers (R7 invite-provider-list): the persistent URL set the helper fans
        // out across, sibling to network_nodes but for MEETING a peer (ntfy/Nostr brokers), not dialing
        // transport. Seeded with the cross-protocol defaults; user/invite-extendable; each removable. An
        // empty set falls back to the compiled default. See docs/decisions/global-identity-discovery.md.
        self.execute_migration_batch(
            "create_rendezvous_providers",
            "
            CREATE TABLE IF NOT EXISTS rendezvous_providers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                url TEXT NOT NULL UNIQUE,
                source TEXT NOT NULL,
                added_at_unix_ms INTEGER NOT NULL
            );
            ",
        )?;

        // Seed the cross-protocol default set exactly once (guarded by an empty-table check, so a user
        // removal sticks): one ntfy HTTP broker + one write-open Nostr relay + the BT-DHT courier
        // (`dht://mainline`) — three DIFFERENT classes (HTTP ⟂ WS ⟂ BitTorrent-DHT) so an outage must
        // block them all. MUST mirror omni_rendezvous::DEFAULT_RENDEZVOUS_PROVIDERS (the SoT const drives
        // the helper/app fallback, this seeds the per-profile editable set) — `dht://mainline` was added
        // to that const (R7 Phase 1/2) but missed here, so the BT-DHT leg never entered the seeded set
        // and silently never ran; added 2026-06-28 to re-sync.
        self.execute_migration_batch(
            "seed_default_rendezvous_providers",
            "
            INSERT INTO rendezvous_providers (url, source, added_at_unix_ms)
            SELECT url, 'base', 0
            FROM (
                SELECT 'https://ntfy.sh' AS url
                UNION ALL SELECT 'wss://nos.lol'
                UNION ALL SELECT 'wss://nostr.mom'
                UNION ALL SELECT 'wss://nostr.oxtr.dev'
                UNION ALL SELECT 'wss://relay.primal.net'
                UNION ALL SELECT 'wss://relay.nostr.net'
                UNION ALL SELECT 'wss://offchain.pub'
                UNION ALL SELECT 'dht://mainline'
            )
            WHERE NOT EXISTS (SELECT 1 FROM rendezvous_providers);
            ",
        )?;

        // The Nostr relay pool (2026-10-10, docs/decisions/courier-metadata-and-quota.md step 4): a profile
        // still on the old default set — exactly ntfy + offchain.pub + Mainline, as seeded — gets the
        // rest of the pool. Keyed on that exact set, it happens once and never undoes a user's own edit
        // (removing or adding any courier takes the profile off the old set). The subqueries are not
        // correlated, so SQLite evaluates them once for the statement, before the first row goes in.
        self.execute_migration_batch(
            "add_nostr_relay_pool_to_the_old_default_set",
            "
            INSERT OR IGNORE INTO rendezvous_providers (url, source, added_at_unix_ms)
            SELECT url, 'base', 0
            FROM (
                SELECT 'wss://nos.lol' AS url
                UNION ALL SELECT 'wss://nostr.mom'
                UNION ALL SELECT 'wss://nostr.oxtr.dev'
                UNION ALL SELECT 'wss://relay.primal.net'
                UNION ALL SELECT 'wss://relay.nostr.net'
            )
            WHERE (SELECT COUNT(*) FROM rendezvous_providers) = 3
              AND (SELECT COUNT(*) FROM rendezvous_providers
                   WHERE source = 'base' AND url IN ('https://ntfy.sh', 'wss://offchain.pub', 'dht://mainline')) = 3;
            ",
        )?;

        // How each courier is used (docs/decisions/courier-metadata-and-quota.md, step 6): post and look,
        // only look, or not at all. A default courier turned off keeps its row, so it can come back.
        self.ensure_table_column("rendezvous_providers", "mode", "TEXT NOT NULL DEFAULT 'publish_read'")?;

        // Outstanding first-contact bootstrap rendezvous invites (persisted so a restart mid-window
        // doesn't silently drop them — they were in-memory only). Stores the invite NONCE (the secret
        // is re-derived from it on load, never written at rest) + the importer's dial node + expiry.
        // See docs/bugs/relay-less-first-contact-validate-before-advertise.md + storage_mesh_bootstrap_invites.rs.
        self.execute_migration_batch(
            "create_mesh_bootstrap_invites",
            "
            CREATE TABLE IF NOT EXISTS mesh_bootstrap_invites (
                nonce_hex TEXT PRIMARY KEY,
                inviter_node BLOB,
                expires_at_ms INTEGER NOT NULL
            );
            ",
        )?;

        // Groups on MLS (docs/decisions/group-crypto-mls.md): membership and keys live in the OpenMLS
        // tables (created by `omni-group-mls`); these are the app's rows around them. The pre-MLS
        // schema (a signed op journal + sender keys) is dropped once: it never carried a group beyond
        // its creator (docs/bugs/groups-do-not-work-past-one-member.md), so nothing is lost.
        //
        // `groups`/`group_members` go only when they ARE the pre-MLS ones (their `state_key`): a
        // `group_ops` alone is no proof — an old build opened on this database re-creates it, and
        // dropping on it wiped the MLS groups' rows (stand 2026-10-11,
        // docs/bugs/old-build-wiped-groups.md).
        if self.table_exists("group_ops")? || self.table_exists("group_sender_keys")? {
            self.execute_migration_batch(
                "drop_pre_mls_group_journal",
                "
                DROP TABLE IF EXISTS group_ops;
                DROP TABLE IF EXISTS group_sender_keys;
                ",
            )?;
        }
        if self.table_exists("groups")? && self.table_column_exists("groups", "state_key")? {
            self.execute_migration_batch(
                "drop_pre_mls_group_tables",
                "
                DROP TABLE IF EXISTS group_members;
                DROP TABLE IF EXISTS groups;
                ",
            )?;
        }
        self.execute_migration_batch(
            "create_groups_tables",
            "
            CREATE TABLE IF NOT EXISTS groups (
                profile_id TEXT NOT NULL,
                group_id TEXT NOT NULL,
                name TEXT NOT NULL,
                my_member_pub TEXT NOT NULL,
                my_status TEXT NOT NULL,
                dialog_key TEXT NOT NULL,
                feed_seq INTEGER NOT NULL DEFAULT 0,
                created_at_unix_ms INTEGER NOT NULL,
                updated_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY (profile_id, group_id)
            );

            CREATE TABLE IF NOT EXISTS group_members (
                profile_id TEXT NOT NULL,
                group_id TEXT NOT NULL,
                member_pub TEXT NOT NULL,
                account_id TEXT NOT NULL,
                display_name TEXT NOT NULL,
                role TEXT NOT NULL,
                status TEXT NOT NULL,
                node_hint_hex TEXT NOT NULL,
                PRIMARY KEY (profile_id, group_id, member_pub)
            );

            CREATE TABLE IF NOT EXISTS group_invites (
                profile_id TEXT NOT NULL,
                group_id TEXT NOT NULL,
                name TEXT NOT NULL,
                inviter_name TEXT NOT NULL,
                inviter_dialog_key TEXT NOT NULL,
                status TEXT NOT NULL,
                received_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY (profile_id, group_id)
            );

            CREATE TABLE IF NOT EXISTS group_pending_inbound (
                profile_id TEXT NOT NULL,
                group_id TEXT NOT NULL,
                epoch INTEGER NOT NULL,
                mls BLOB NOT NULL,
                received_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY (profile_id, group_id, mls)
            );

            CREATE TABLE IF NOT EXISTS group_outbox (
                profile_id TEXT NOT NULL,
                group_id TEXT NOT NULL,
                epoch INTEGER NOT NULL,
                message_id TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                sent_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY (profile_id, group_id, message_id)
            );

            CREATE TABLE IF NOT EXISTS group_own_changes (
                profile_id TEXT NOT NULL,
                group_id TEXT NOT NULL,
                epoch_after INTEGER NOT NULL,
                change_json TEXT NOT NULL,
                PRIMARY KEY (profile_id, group_id, epoch_after)
            );

            CREATE TABLE IF NOT EXISTS group_message_meta (
                profile_id TEXT NOT NULL,
                group_id TEXT NOT NULL,
                message_id TEXT NOT NULL,
                sender_pub TEXT,
                sender_name TEXT NOT NULL,
                kind TEXT NOT NULL,
                PRIMARY KEY (profile_id, message_id)
            );

            CREATE TABLE IF NOT EXISTS group_feed_outbox (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                profile_id TEXT NOT NULL,
                group_id TEXT NOT NULL,
                mls BLOB NOT NULL,
                created_at_unix_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS group_sent_invites (
                profile_id TEXT NOT NULL,
                group_id TEXT NOT NULL,
                dialog_key TEXT NOT NULL,
                sent_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY (profile_id, group_id, dialog_key)
            );

            -- What a group member told the group about itself (docs/decisions/group-member-profile.md):
            -- one row per person, whatever groups we share; the photo is a file named by its sha256.
            CREATE TABLE IF NOT EXISTS group_member_profiles (
                profile_id TEXT NOT NULL,
                account_id TEXT NOT NULL,
                version INTEGER NOT NULL,
                display_name TEXT NOT NULL,
                avatar_hash TEXT,
                updated_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY (profile_id, account_id)
            );

            CREATE TABLE IF NOT EXISTS mls_storage_key (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                protected_json TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS contact_remote_access (
                profile_id TEXT NOT NULL,
                contact_id TEXT NOT NULL,
                granted_mode TEXT,
                granted_changed_at_unix_ms INTEGER,
                received_mode TEXT,
                received_changed_at_unix_ms INTEGER,
                last_session_at_unix_ms INTEGER,
                PRIMARY KEY (profile_id, contact_id)
            );
            ",
        )?;
        // The group's picture (its sha256; the bytes are a file in the profile media, see
        // storage_groups.rs). Set by an admin in the group's MLS meta.
        if !self.table_column_exists("groups", "avatar_hash")? {
            self.execute_migration_batch(
                "groups_add_avatar_hash",
                "
                ALTER TABLE groups ADD COLUMN avatar_hash TEXT;
                ",
            )?;
        }

        // What this device has read of each chat (storage_read_marks.rs). Made on an existing
        // profile, it starts with every chat read: the history so far is not news.
        if !self.table_exists("dialog_read_marks")? {
            self.execute_migration_batch(
                "create_dialog_read_marks",
                "
                CREATE TABLE dialog_read_marks (
                    profile_id TEXT NOT NULL,
                    dialog_key TEXT NOT NULL,
                    read_through_unix_ms INTEGER NOT NULL,
                    updated_at_unix_ms INTEGER NOT NULL,
                    PRIMARY KEY (profile_id, dialog_key)
                );
                INSERT INTO dialog_read_marks (profile_id, dialog_key, read_through_unix_ms, updated_at_unix_ms)
                SELECT profile_id, dialog_key, MAX(created_at_unix_ms), 0
                FROM messages
                WHERE profile_id IS NOT NULL AND dialog_key IS NOT NULL AND dialog_key <> ''
                GROUP BY profile_id, dialog_key;
                ",
            )?;
        }

        self.purge_legacy_contact_artifacts()?;

        self.connection
            .execute(
                "INSERT INTO app_meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![SCHEMA_VERSION_KEY, SCHEMA_VERSION.to_string()],
            )
            .map_err(map_sqlite_error)?;

        Ok(())
        })();

        match migration_result {
            Ok(()) => {
                if started_transaction {
                    self.connection.execute_batch("COMMIT").map_err(|error| {
                        OmniError::Storage(format!(
                            "failed to commit migration transaction: {error}"
                        ))
                    })?;
                }
                // Every profile whose root opens here gets a recovery-phrase wrap of the database key
                // (a database encrypted by this build's first open has none yet).
                self.ensure_database_key_wraps()?;
                Ok(())
            }
            Err(error) => {
                if started_transaction {
                    let _ = self.connection.execute_batch("ROLLBACK");
                }
                Err(error)
            }
        }
    }

    fn migrate_prekey_tables_to_device_scoped_primary_keys(&self) -> OmniResult<()> {
        self.rebuild_signed_prekeys_table_with_device_scoped_primary_key()?;
        self.rebuild_one_time_prekeys_table_with_device_scoped_primary_key()?;
        Ok(())
    }

    fn rebuild_signed_prekeys_table_with_device_scoped_primary_key(&self) -> OmniResult<()> {
        if !self.table_exists("signed_prekeys")? {
            return Ok(());
        }
        if !self.prekey_table_uses_legacy_global_primary_key("signed_prekeys")? {
            self.connection
                .execute(
                    "
                    CREATE INDEX IF NOT EXISTS idx_signed_prekeys_device_state
                    ON signed_prekeys(device_id, state, created_at_unix_ms DESC, prekey_id DESC)
                    ",
                    [],
                )
                .map_err(map_sqlite_error)?;
            return Ok(());
        }

        self.execute_migration_batch(
            "rebuild_signed_prekeys_device_scoped_primary_key",
            "
            ALTER TABLE signed_prekeys RENAME TO signed_prekeys_legacy_global_pk;

            CREATE TABLE signed_prekeys (
                device_id TEXT NOT NULL,
                prekey_id INTEGER NOT NULL,
                public_key TEXT NOT NULL,
                private_key_protection_scheme TEXT NOT NULL,
                private_key_blob TEXT NOT NULL,
                signature_r TEXT NOT NULL,
                signature_s TEXT NOT NULL,
                suite_version INTEGER NOT NULL,
                state TEXT NOT NULL DEFAULT 'active',
                created_at_unix_ms INTEGER NOT NULL,
                expires_at_unix_ms INTEGER,
                updated_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY (device_id, prekey_id),
                FOREIGN KEY(device_id) REFERENCES local_devices(device_id)
            );

            INSERT INTO signed_prekeys (
                device_id,
                prekey_id,
                public_key,
                private_key_protection_scheme,
                private_key_blob,
                signature_r,
                signature_s,
                suite_version,
                state,
                created_at_unix_ms,
                expires_at_unix_ms,
                updated_at_unix_ms
            )
            SELECT
                device_id,
                prekey_id,
                public_key,
                private_key_protection_scheme,
                private_key_blob,
                signature_r,
                signature_s,
                suite_version,
                state,
                created_at_unix_ms,
                expires_at_unix_ms,
                updated_at_unix_ms
            FROM signed_prekeys_legacy_global_pk;

            DROP TABLE signed_prekeys_legacy_global_pk;

            CREATE INDEX IF NOT EXISTS idx_signed_prekeys_device_state
            ON signed_prekeys(device_id, state, created_at_unix_ms DESC, prekey_id DESC);
            ",
        )
    }

    fn rebuild_one_time_prekeys_table_with_device_scoped_primary_key(&self) -> OmniResult<()> {
        if !self.table_exists("one_time_prekeys")? {
            return Ok(());
        }
        if !self.prekey_table_uses_legacy_global_primary_key("one_time_prekeys")? {
            self.connection
                .execute(
                    "
                    CREATE INDEX IF NOT EXISTS idx_one_time_prekeys_device_state
                    ON one_time_prekeys(device_id, state, created_at_unix_ms ASC, prekey_id ASC)
                    ",
                    [],
                )
                .map_err(map_sqlite_error)?;
            return Ok(());
        }

        self.execute_migration_batch(
            "rebuild_one_time_prekeys_device_scoped_primary_key",
            "
            ALTER TABLE one_time_prekeys RENAME TO one_time_prekeys_legacy_global_pk;

            CREATE TABLE one_time_prekeys (
                device_id TEXT NOT NULL,
                prekey_id INTEGER NOT NULL,
                public_key TEXT NOT NULL,
                private_key_protection_scheme TEXT NOT NULL,
                private_key_blob TEXT NOT NULL,
                suite_version INTEGER NOT NULL,
                state TEXT NOT NULL DEFAULT 'available',
                created_at_unix_ms INTEGER NOT NULL,
                consumed_at_unix_ms INTEGER,
                updated_at_unix_ms INTEGER NOT NULL,
                PRIMARY KEY (device_id, prekey_id),
                FOREIGN KEY(device_id) REFERENCES local_devices(device_id)
            );

            INSERT INTO one_time_prekeys (
                device_id,
                prekey_id,
                public_key,
                private_key_protection_scheme,
                private_key_blob,
                suite_version,
                state,
                created_at_unix_ms,
                consumed_at_unix_ms,
                updated_at_unix_ms
            )
            SELECT
                device_id,
                prekey_id,
                public_key,
                private_key_protection_scheme,
                private_key_blob,
                suite_version,
                state,
                created_at_unix_ms,
                consumed_at_unix_ms,
                updated_at_unix_ms
            FROM one_time_prekeys_legacy_global_pk;

            DROP TABLE one_time_prekeys_legacy_global_pk;

            CREATE INDEX IF NOT EXISTS idx_one_time_prekeys_device_state
            ON one_time_prekeys(device_id, state, created_at_unix_ms ASC, prekey_id ASC);
            ",
        )
    }

    fn prekey_table_uses_legacy_global_primary_key(&self, table_name: &str) -> OmniResult<bool> {
        let mut statement = self
            .connection
            .prepare(&format!("PRAGMA table_info({table_name})"))
            .map_err(map_sqlite_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .map_err(map_sqlite_error)?;
        let mut primary_key_columns = Vec::new();
        for row in rows {
            let (column_name, primary_key_position) = row.map_err(map_sqlite_error)?;
            if primary_key_position > 0 {
                primary_key_columns.push((primary_key_position, column_name));
            }
        }
        primary_key_columns.sort_by_key(|(position, _)| *position);
        Ok(matches!(
            primary_key_columns.as_slice(),
            [(1, column_name)] if column_name == "prekey_id"
        ))
    }

    fn purge_legacy_contact_artifacts(&self) -> OmniResult<()> {
        self.connection
            .execute(
                "
                DELETE FROM contact_dialog_links
                WHERE contact_id IN (
                    SELECT contact_id
                    FROM contacts
                    WHERE removed_at_unix_ms IS NOT NULL
                       OR remote_account_id = (
                           SELECT pr.account_id
                           FROM profile_registry pr
                           WHERE pr.profile_id = contacts.profile_id
                           LIMIT 1
                       )
                )
                ",
                [],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                DELETE FROM contact_verifications
                WHERE contact_id IN (
                    SELECT contact_id
                    FROM contacts
                    WHERE removed_at_unix_ms IS NOT NULL
                       OR remote_account_id = (
                           SELECT pr.account_id
                           FROM profile_registry pr
                           WHERE pr.profile_id = contacts.profile_id
                           LIMIT 1
                       )
                )
                ",
                [],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                DELETE FROM contact_identity_observations
                WHERE contact_id IN (
                    SELECT contact_id
                    FROM contacts
                    WHERE removed_at_unix_ms IS NOT NULL
                       OR remote_account_id = (
                           SELECT pr.account_id
                           FROM profile_registry pr
                           WHERE pr.profile_id = contacts.profile_id
                           LIMIT 1
                       )
                )
                ",
                [],
            )
            .map_err(map_sqlite_error)?;
        self.connection
            .execute(
                "
                DELETE FROM contacts
                WHERE removed_at_unix_ms IS NOT NULL
                   OR remote_account_id = (
                       SELECT pr.account_id
                       FROM profile_registry pr
                       WHERE pr.profile_id = contacts.profile_id
                       LIMIT 1
                   )
                ",
                [],
            )
            .map_err(map_sqlite_error)?;

        Ok(())
    }
}
