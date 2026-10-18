---
tags: [architecture, storage, schema, auto-generated]
---

# Storage Schema

**Auto-generated. Do not edit.** Regenerate with `scripts/regen-docs.ps1`.

Tables defined by CREATE TABLE in omni-storage/src/storage_migrations.rs.
Additive columns added via ensure_table_column are listed under each table.

## Table index

- [`account_devices`](#account-devices)
- [`account_key_moves`](#account-key-moves)
- [`account_keys`](#account-keys)
- [`account_working_keys`](#account-working-keys)
- [`app_meta`](#app-meta)
- [`call_participants`](#call-participants)
- [`call_sessions`](#call-sessions)
- [`chat_history_checks`](#chat-history-checks)
- [`contact_dialog_links`](#contact-dialog-links)
- [`contact_identity_observations`](#contact-identity-observations)
- [`contact_presence`](#contact-presence)
- [`contact_remote_access`](#contact-remote-access)
- [`contact_sync_device_marks`](#contact-sync-device-marks)
- [`contact_sync_records`](#contact-sync-records)
- [`contact_verifications`](#contact-verifications)
- [`contacts`](#contacts)
- [`delivery_receipts`](#delivery-receipts)
- [`device_keys`](#device-keys)
- [`device_revocations`](#device-revocations)
- [`dialog_read_marks`](#dialog-read-marks)
- [`durable_feed_seq`](#durable-feed-seq)
- [`durable_frames`](#durable-frames)
- [`group_branches`](#group-branches)
- [`group_feed_outbox`](#group-feed-outbox)
- [`group_invites`](#group-invites)
- [`group_member_profiles`](#group-member-profiles)
- [`group_members`](#group-members)
- [`group_message_meta`](#group-message-meta)
- [`group_outbox`](#group-outbox)
- [`group_own_changes`](#group-own-changes)
- [`group_pending_inbound`](#group-pending-inbound)
- [`group_pending_welcomes`](#group-pending-welcomes)
- [`group_readd_requests`](#group-readd-requests)
- [`group_sent_invites`](#group-sent-invites)
- [`group_signed_lines`](#group-signed-lines)
- [`groups`](#groups)
- [`held_passports`](#held-passports)
- [`history_imports`](#history-imports)
- [`identity_keys`](#identity-keys)
- [`inbound_dedup`](#inbound-dedup)
- [`local_account`](#local-account)
- [`local_devices`](#local-devices)
- [`mesh_bootstrap_invites`](#mesh-bootstrap-invites)
- [`mesh_dialog_route`](#mesh-dialog-route)
- [`mesh_peer_book`](#mesh-peer-book)
- [`message_attachments`](#message-attachments)
- [`message_device_receipts`](#message-device-receipts)
- [`message_security_metadata`](#message-security-metadata)
- [`messages`](#messages)
- [`mls_storage_key`](#mls-storage-key)
- [`network_nodes`](#network-nodes)
- [`one_time_prekeys`](#one-time-prekeys)
- [`outbound_queue`](#outbound-queue)
- [`own_passport`](#own-passport)
- [`passport_holders`](#passport-holders)
- [`peer_courier_sets`](#peer-courier-sets)
- [`pending_key_move_devices`](#pending-key-move-devices)
- [`pending_key_move_seeds`](#pending-key-move-seeds)
- [`pending_working_keys`](#pending-working-keys)
- [`profile_current_device`](#profile-current-device)
- [`profile_registry`](#profile-registry)
- [`profile_secrets`](#profile-secrets)
- [`recovered_groups`](#recovered-groups)
- [`remote_device_observations`](#remote-device-observations)
- [`remote_prekey_bundles`](#remote-prekey-bundles)
- [`rendezvous_providers`](#rendezvous-providers)
- [`replay_windows`](#replay-windows)
- [`sessions`](#sessions)
- [`signed_prekeys`](#signed-prekeys)

## `account_devices`

| Column / Constraint | Definition |
|---|---|
| `account_id` | TEXT NOT NULL |
| `device_id` | TEXT NOT NULL |
| `certificate_chain_json` | TEXT NOT NULL |
| `device_label` | TEXT NOT NULL |
| `first_seen_at_unix_ms` | INTEGER NOT NULL |
| `revoked_at_unix_ms` | INTEGER |
| _constraint_ | PRIMARY KEY (account_id, device_id) |

**Added via `ensure_table_column` (additive migrations):**

| Column | Type |
|---|---|
| `dialog_key` | TEXT |
| `last_heard_at_unix_ms` | INTEGER |

## `account_key_moves`

| Column / Constraint | Definition |
|---|---|
| `account_id` | TEXT PRIMARY KEY NOT NULL |
| `version` | INTEGER NOT NULL |
| `move_json` | TEXT NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |

## `account_keys`

| Column / Constraint | Definition |
|---|---|
| `account_id` | TEXT PRIMARY KEY NOT NULL |
| `master_public_key` | TEXT NOT NULL |
| `working_key_version` | INTEGER NOT NULL |
| `working_key_protection_scheme` | TEXT NOT NULL |
| `working_key_blob` | TEXT NOT NULL |
| `working_key_certificate_json` | TEXT NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |

**Added via `ensure_table_column` (additive migrations):**

| Column | Type |
|---|---|
| `rendezvous_secret_protection_scheme` | TEXT |
| `rendezvous_secret_blob` | TEXT |

## `account_working_keys`

| Column / Constraint | Definition |
|---|---|
| `account_id` | TEXT PRIMARY KEY NOT NULL |
| `version` | INTEGER NOT NULL |
| `certificate_json` | TEXT NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |

## `app_meta`

| Column / Constraint | Definition |
|---|---|
| `key` | TEXT PRIMARY KEY NOT NULL |
| `value` | TEXT NOT NULL |

## `call_participants`

| Column / Constraint | Definition |
|---|---|
| `call_id` | TEXT NOT NULL |
| `participant_id` | TEXT NOT NULL |
| `account_id` | TEXT NOT NULL |
| `device_id` | TEXT NOT NULL |
| `display_name` | TEXT |
| `is_local` | INTEGER NOT NULL DEFAULT 0 |
| `is_muted` | INTEGER NOT NULL DEFAULT 0 |
| _constraint_ | PRIMARY KEY(call_id, participant_id) |
| _constraint_ | FOREIGN KEY(call_id) REFERENCES call_sessions(call_id) ON DELETE CASCADE |

## `call_sessions`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `call_id` | TEXT PRIMARY KEY NOT NULL |
| `dialog_key` | TEXT NOT NULL |
| `contact_id` | TEXT |
| `direction` | TEXT NOT NULL |
| `state` | TEXT NOT NULL |
| `outcome` | TEXT |
| `decline_reason` | TEXT |
| `local_participant_id` | TEXT |
| `remote_participant_id` | TEXT |
| `initiator_account_id` | TEXT |
| `initiator_device_id` | TEXT |
| `responder_account_id` | TEXT |
| `responder_device_id` | TEXT |
| `local_mic_muted` | INTEGER NOT NULL DEFAULT 0 |
| `network_quality` | TEXT |
| `started_at_unix_ms` | INTEGER NOT NULL |
| `ringing_started_at_unix_ms` | INTEGER |
| `connected_at_unix_ms` | INTEGER |
| `ended_at_unix_ms` | INTEGER |
| `reconnect_deadline_unix_ms` | INTEGER |
| `updated_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | FOREIGN KEY(contact_id) REFERENCES contacts(contact_id) |

## `chat_history_checks`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `chat_account_id` | TEXT NOT NULL |
| `state` | TEXT NOT NULL |
| `checked_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | PRIMARY KEY (profile_id, chat_account_id) |

## `contact_dialog_links`

| Column / Constraint | Definition |
|---|---|
| `contact_id` | TEXT NOT NULL |
| `dialog_key` | TEXT NOT NULL |
| `is_primary` | INTEGER NOT NULL DEFAULT 0 |
| `created_at_unix_ms` | INTEGER NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | PRIMARY KEY (contact_id, dialog_key) |
| _constraint_ | FOREIGN KEY(contact_id) REFERENCES contacts(contact_id) |

**Added via `ensure_table_column` (additive migrations):**

| Column | Type |
|---|---|
| `profile_id` | TEXT |
| `remote_device_id` | TEXT |

## `contact_identity_observations`

| Column / Constraint | Definition |
|---|---|
| `contact_id` | TEXT PRIMARY KEY NOT NULL |
| `identity_key` | TEXT NOT NULL |
| `observed_display_name` | TEXT |
| `observed_at_unix_ms` | INTEGER NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | FOREIGN KEY(contact_id) REFERENCES contacts(contact_id) |

**Added via `ensure_table_column` (additive migrations):**

| Column | Type |
|---|---|
| `profile_id` | TEXT |

## `contact_presence`

| Column / Constraint | Definition |
|---|---|
| `contact_id` | TEXT NOT NULL |
| `profile_id` | TEXT NOT NULL |
| `last_seen_at_unix_ms` | INTEGER |
| `last_sent_at_unix_ms` | INTEGER |
| `created_at_unix_ms` | INTEGER NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | PRIMARY KEY (contact_id, profile_id) |

**Added via `ensure_table_column` (additive migrations):**

| Column | Type |
|---|---|
| `presence_state` | TEXT NOT NULL DEFAULT 'offline' |
| `last_activity_at_unix_ms` | INTEGER |
| `consecutive_missed_heartbeats` | INTEGER NOT NULL DEFAULT 0 |
| `next_heartbeat_due_at_unix_ms` | INTEGER |

## `contact_remote_access`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `contact_id` | TEXT NOT NULL |
| `granted_mode` | TEXT |
| `granted_changed_at_unix_ms` | INTEGER |
| `received_mode` | TEXT |
| `received_changed_at_unix_ms` | INTEGER |
| `last_session_at_unix_ms` | INTEGER |
| _constraint_ | PRIMARY KEY (profile_id, contact_id) |

## `contact_sync_device_marks`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `device_id` | TEXT NOT NULL |
| `acked_seq` | INTEGER NOT NULL DEFAULT 0 |
| `sent_through_seq` | INTEGER NOT NULL DEFAULT 0 |
| `sent_at_unix_ms` | INTEGER NOT NULL DEFAULT 0 |
| _constraint_ | PRIMARY KEY (profile_id, device_id) |

## `contact_sync_records`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `remote_account_id` | TEXT NOT NULL |
| `state` | TEXT NOT NULL |
| `state_version` | INTEGER NOT NULL |
| `state_writer` | TEXT NOT NULL |
| `petname` | TEXT NOT NULL |
| `petname_version` | INTEGER NOT NULL |
| `petname_writer` | TEXT NOT NULL |
| `display_name` | TEXT |
| `seq` | INTEGER NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | PRIMARY KEY (profile_id, remote_account_id) |

## `contact_verifications`

| Column / Constraint | Definition |
|---|---|
| `contact_id` | TEXT PRIMARY KEY NOT NULL |
| `verified_at_unix_ms` | INTEGER NOT NULL |
| `verification_method` | TEXT NOT NULL |
| `verified_identity_key` | TEXT |
| `updated_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | FOREIGN KEY(contact_id) REFERENCES contacts(contact_id) |

**Added via `ensure_table_column` (additive migrations):**

| Column | Type |
|---|---|
| `profile_id` | TEXT |

## `contacts`

| Column / Constraint | Definition |
|---|---|
| `contact_id` | TEXT PRIMARY KEY NOT NULL |
| `petname` | TEXT NOT NULL |
| `remote_account_id` | TEXT |
| `public_display_name` | TEXT |
| `public_avatar_pending_hash_hex` | TEXT |
| `public_avatar_pending_mime_type` | TEXT |
| `remote_instance_id` | TEXT |
| `relationship_state` | TEXT NOT NULL DEFAULT 'approved' |
| `pending_request_direction` | TEXT |
| `is_blocked` | INTEGER NOT NULL DEFAULT 0 |
| `removed_at_unix_ms` | INTEGER |
| `created_at_unix_ms` | INTEGER NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |

**Added via `ensure_table_column` (additive migrations):**

| Column | Type |
|---|---|
| `remote_account_id` | TEXT |
| `public_avatar_hash_hex` | TEXT |
| `public_avatar_mime_type` | TEXT |
| `public_profile_updated_at_unix_ms` | INTEGER |
| `relationship_state` | TEXT NOT NULL DEFAULT 'approved' |
| `pending_request_direction` | TEXT |
| `profile_id` | TEXT |
| `public_avatar_pending_hash_hex` | TEXT |
| `public_avatar_pending_mime_type` | TEXT |
| `has_met` | INTEGER NOT NULL DEFAULT 0 |
| `friend_relay_consent` | INTEGER NOT NULL DEFAULT 0 |

## `delivery_receipts`

| Column / Constraint | Definition |
|---|---|
| `message_id` | TEXT PRIMARY KEY NOT NULL |
| `dialog_key` | TEXT NOT NULL |
| `delivered_at_unix_ms` | INTEGER NOT NULL |
| `recorded_at_unix_ms` | INTEGER NOT NULL |

**Added via `ensure_table_column` (additive migrations):**

| Column | Type |
|---|---|
| `profile_id` | TEXT |

## `device_keys`

| Column / Constraint | Definition |
|---|---|
| `device_id` | TEXT PRIMARY KEY NOT NULL |
| `account_id` | TEXT NOT NULL |
| `public_key` | TEXT NOT NULL |
| `private_key_protection_scheme` | TEXT NOT NULL |
| `private_key_blob` | TEXT NOT NULL |
| `suite_version` | INTEGER NOT NULL |
| `created_at_unix_ms` | INTEGER NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | FOREIGN KEY(device_id) REFERENCES local_devices(device_id) |
| _constraint_ | FOREIGN KEY(account_id) REFERENCES local_account(account_id) |

## `device_revocations`

| Column / Constraint | Definition |
|---|---|
| `account_id` | TEXT NOT NULL |
| `device_id` | TEXT NOT NULL |
| `revocation_json` | TEXT NOT NULL |
| `working_key_version` | INTEGER NOT NULL |
| `revoked_at_unix_ms` | INTEGER NOT NULL |
| `received_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | PRIMARY KEY (account_id, device_id) |

## `dialog_read_marks`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `dialog_key` | TEXT NOT NULL |
| `read_through_unix_ms` | INTEGER NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | PRIMARY KEY (profile_id, dialog_key) |

## `durable_feed_seq`

| Column / Constraint | Definition |
|---|---|
| `peer` | BLOB PRIMARY KEY NOT NULL |
| `seq` | INTEGER NOT NULL |

## `durable_frames`

| Column / Constraint | Definition |
|---|---|
| `feed_id` | BLOB NOT NULL |
| `seq` | INTEGER NOT NULL |
| `wire` | BLOB NOT NULL |
| _constraint_ | PRIMARY KEY (feed_id, seq) |

## `group_branches`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `group_id` | TEXT NOT NULL |
| `fork_epoch` | INTEGER NOT NULL |
| `ours_wins` | INTEGER NOT NULL |
| `first_seen_at_unix_ms` | INTEGER NOT NULL |
| `rejoining_since_unix_ms` | INTEGER |
| `requested_at_unix_ms` | INTEGER NOT NULL DEFAULT 0 |
| `request_mls` | BLOB |
| _constraint_ | PRIMARY KEY (profile_id, group_id) |

## `group_feed_outbox`

| Column / Constraint | Definition |
|---|---|
| `id` | INTEGER PRIMARY KEY AUTOINCREMENT |
| `profile_id` | TEXT NOT NULL |
| `group_id` | TEXT NOT NULL |
| `mls` | BLOB NOT NULL |
| `created_at_unix_ms` | INTEGER NOT NULL |

## `group_invites`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `group_id` | TEXT NOT NULL |
| `name` | TEXT NOT NULL |
| `inviter_name` | TEXT NOT NULL |
| `inviter_dialog_key` | TEXT NOT NULL |
| `status` | TEXT NOT NULL |
| `received_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | PRIMARY KEY (profile_id, group_id) |

## `group_member_profiles`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `account_id` | TEXT NOT NULL |
| `version` | INTEGER NOT NULL |
| `display_name` | TEXT NOT NULL |
| `avatar_hash` | TEXT |
| `updated_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | PRIMARY KEY (profile_id, account_id) |

## `group_members`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `group_id` | TEXT NOT NULL |
| `member_pub` | TEXT NOT NULL |
| `account_id` | TEXT NOT NULL |
| `display_name` | TEXT NOT NULL |
| `role` | TEXT NOT NULL |
| `status` | TEXT NOT NULL |
| `node_hint_hex` | TEXT NOT NULL |
| _constraint_ | PRIMARY KEY (profile_id, group_id, member_pub) |

## `group_message_meta`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `group_id` | TEXT NOT NULL |
| `message_id` | TEXT NOT NULL |
| `sender_pub` | TEXT |
| `sender_name` | TEXT NOT NULL |
| `kind` | TEXT NOT NULL |
| _constraint_ | PRIMARY KEY (profile_id, message_id) |

## `group_outbox`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `group_id` | TEXT NOT NULL |
| `epoch` | INTEGER NOT NULL |
| `message_id` | TEXT NOT NULL |
| `payload_json` | TEXT NOT NULL |
| `sent_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | PRIMARY KEY (profile_id, group_id, message_id) |

## `group_own_changes`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `group_id` | TEXT NOT NULL |
| `epoch_after` | INTEGER NOT NULL |
| `change_json` | TEXT NOT NULL |
| _constraint_ | PRIMARY KEY (profile_id, group_id, epoch_after) |

## `group_pending_inbound`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `group_id` | TEXT NOT NULL |
| `epoch` | INTEGER NOT NULL |
| `mls` | BLOB NOT NULL |
| `received_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | PRIMARY KEY (profile_id, group_id, mls) |

## `group_pending_welcomes`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `group_id` | TEXT NOT NULL |
| `member_pub` | TEXT NOT NULL |
| `epoch_before` | INTEGER NOT NULL |
| `commit_rank` | BLOB NOT NULL |
| `welcome` | BLOB NOT NULL |
| `created_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | PRIMARY KEY (profile_id, group_id, member_pub) |

## `group_readd_requests`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `group_id` | TEXT NOT NULL |
| `member_pub` | TEXT NOT NULL |
| `key_package` | BLOB NOT NULL |
| `seen_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | PRIMARY KEY (profile_id, group_id, member_pub) |

## `group_sent_invites`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `group_id` | TEXT NOT NULL |
| `dialog_key` | TEXT NOT NULL |
| `sent_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | PRIMARY KEY (profile_id, group_id, dialog_key) |

## `group_signed_lines`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `group_id` | TEXT NOT NULL |
| `message_id` | TEXT NOT NULL |
| `kind` | TEXT NOT NULL |
| `sender_pub` | TEXT NOT NULL |
| `sender_name` | TEXT NOT NULL |
| `body` | TEXT NOT NULL |
| `authored_at_unix_ms` | INTEGER NOT NULL |
| `signature_b64` | TEXT NOT NULL |
| _constraint_ | PRIMARY KEY (profile_id, message_id) |

## `groups`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `group_id` | TEXT NOT NULL |
| `name` | TEXT NOT NULL |
| `my_member_pub` | TEXT NOT NULL |
| `my_status` | TEXT NOT NULL |
| `dialog_key` | TEXT NOT NULL |
| `feed_seq` | INTEGER NOT NULL DEFAULT 0 |
| `created_at_unix_ms` | INTEGER NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | PRIMARY KEY (profile_id, group_id) |

## `held_passports`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `owner_account_id` | TEXT NOT NULL |
| `locator_hex` | TEXT NOT NULL |
| `version` | INTEGER NOT NULL |
| `sealed` | BLOB NOT NULL |
| `mark_secret_hex` | TEXT NOT NULL |
| `stored_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | PRIMARY KEY (profile_id, owner_account_id) |

## `history_imports`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `message_id` | TEXT NOT NULL |
| `source_account_id` | TEXT NOT NULL |
| `imported_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | PRIMARY KEY (profile_id, message_id) |

## `identity_keys`

| Column / Constraint | Definition |
|---|---|
| `account_id` | TEXT PRIMARY KEY NOT NULL |
| `public_key` | TEXT NOT NULL |
| `private_key_protection_scheme` | TEXT NOT NULL |
| `private_key_blob` | TEXT NOT NULL |
| `suite_version` | INTEGER NOT NULL |
| `created_at_unix_ms` | INTEGER NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | FOREIGN KEY(account_id) REFERENCES local_account(account_id) |

**Added via `ensure_table_column` (additive migrations):**

| Column | Type |
|---|---|
| `signing_public_key` | TEXT |
| `signing_private_key_protection_scheme` | TEXT |
| `signing_private_key_blob` | TEXT |

## `inbound_dedup`

| Column / Constraint | Definition |
|---|---|
| `message_id` | TEXT PRIMARY KEY NOT NULL |
| `dialog_key` | TEXT NOT NULL |
| `first_seen_unix_ms` | INTEGER NOT NULL |

**Added via `ensure_table_column` (additive migrations):**

| Column | Type |
|---|---|
| `profile_id` | TEXT |

## `local_account`

| Column / Constraint | Definition |
|---|---|
| `singleton` | INTEGER NOT NULL DEFAULT 1 |
| `account_id` | TEXT PRIMARY KEY NOT NULL |
| `display_name` | TEXT NOT NULL |
| `recovery_version` | INTEGER NOT NULL |
| `recovery_code` | TEXT NOT NULL |
| `restored_from_recovery` | INTEGER NOT NULL DEFAULT 0 |
| `created_at_unix_ms` | INTEGER NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |

**Added via `ensure_table_column` (additive migrations):**

| Column | Type |
|---|---|
| `account_representation_version` | INTEGER NOT NULL DEFAULT 1 |
| `profile_id` | TEXT |
| `profile_state` | TEXT NOT NULL DEFAULT 'signed_in' |
| `public_profile_updated_at_unix_ms` | INTEGER NOT NULL DEFAULT 0 |
| `public_avatar_hash_hex` | TEXT |
| `public_avatar_mime_type` | TEXT |

## `local_devices`

| Column / Constraint | Definition |
|---|---|
| `device_id` | TEXT PRIMARY KEY NOT NULL |
| `account_id` | TEXT NOT NULL |
| `device_label` | TEXT NOT NULL |
| `is_current` | INTEGER NOT NULL DEFAULT 0 |
| `restored_from_recovery` | INTEGER NOT NULL DEFAULT 0 |
| `created_at_unix_ms` | INTEGER NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | FOREIGN KEY(account_id) REFERENCES local_account(account_id) |

**Added via `ensure_table_column` (additive migrations):**

| Column | Type |
|---|---|
| `bound_db_path` | TEXT |

## `mesh_bootstrap_invites`

| Column / Constraint | Definition |
|---|---|
| `nonce_hex` | TEXT PRIMARY KEY |
| `inviter_node` | BLOB |
| `expires_at_ms` | INTEGER NOT NULL |

## `mesh_dialog_route`

| Column / Constraint | Definition |
|---|---|
| `node_address` | BLOB PRIMARY KEY NOT NULL |
| `dialog_key` | TEXT NOT NULL |

## `mesh_peer_book`

| Column / Constraint | Definition |
|---|---|
| `node_address` | BLOB NOT NULL |
| `dial_addr` | TEXT NOT NULL |
| `observed_at_unix_ms` | INTEGER NOT NULL |
| `expires_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | PRIMARY KEY (node_address, dial_addr) |

## `message_attachments`

| Column / Constraint | Definition |
|---|---|
| `message_id` | TEXT PRIMARY KEY NOT NULL |
| `dialog_key` | TEXT NOT NULL |
| `file_name` | TEXT NOT NULL |
| `file_size_bytes` | INTEGER NOT NULL |
| `mime_type` | TEXT |
| `content_id_hex` | TEXT NOT NULL |
| `state` | TEXT NOT NULL |
| `bytes_transferred` | INTEGER NOT NULL DEFAULT 0 |
| `local_blob_path` | TEXT |
| `local_serving_enabled` | INTEGER NOT NULL DEFAULT 0 |
| `remote_holder_available` | INTEGER NOT NULL DEFAULT 0 |
| `local_saved_path` | TEXT |
| `temp_download_path` | TEXT |
| `last_error` | TEXT |
| `created_at_unix_ms` | INTEGER NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |
| `profile_id` | TEXT |

**Added via `ensure_table_column` (additive migrations):**

| Column | Type |
|---|---|
| `content_tree_blocks` | BLOB |
| `content_blocks_have` | BLOB |
| `profile_id` | TEXT |
| `content_encryption` | TEXT |
| `content_key_hex` | TEXT |
| `content_nonce_seed_hex` | TEXT |
| `content_chunk_size_bytes` | INTEGER |
| `active_transfer_id` | TEXT |
| `local_blob_managed` | INTEGER NOT NULL DEFAULT 1 |
| `local_source_size_bytes` | INTEGER |
| `local_source_modified_unix_ms` | INTEGER |
| `presentation_json` | TEXT |

## `message_device_receipts`

| Column / Constraint | Definition |
|---|---|
| `message_id` | TEXT NOT NULL |
| `remote_device_id` | TEXT NOT NULL |
| `delivered_at_unix_ms` | INTEGER NOT NULL |
| `profile_id` | TEXT NOT NULL |
| _constraint_ | PRIMARY KEY (message_id, remote_device_id) |

## `message_security_metadata`

| Column / Constraint | Definition |
|---|---|
| `message_id` | TEXT PRIMARY KEY NOT NULL |
| `dialog_key` | TEXT NOT NULL |
| `contact_id` | TEXT |
| `session_address` | TEXT |
| `remote_account_id` | TEXT |
| `remote_device_id` | TEXT |
| `encryption_state` | TEXT NOT NULL |
| `bootstrap_state` | TEXT |
| `payload_kind` | TEXT |
| `created_at_unix_ms` | INTEGER NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | FOREIGN KEY(contact_id) REFERENCES contacts(contact_id) |

**Added via `ensure_table_column` (additive migrations):**

| Column | Type |
|---|---|
| `profile_id` | TEXT |

## `messages`

| Column / Constraint | Definition |
|---|---|
| `id` | TEXT PRIMARY KEY NOT NULL |
| `body` | TEXT NOT NULL |
| `status` | TEXT NOT NULL |
| `created_at_unix_ms` | INTEGER NOT NULL |

**Added via `ensure_table_column` (additive migrations):**

| Column | Type |
|---|---|
| `profile_id` | TEXT |

## `mls_storage_key`

| Column / Constraint | Definition |
|---|---|
| `id` | INTEGER PRIMARY KEY CHECK (id = 1) |
| `protected_json` | TEXT NOT NULL |

## `network_nodes`

| Column / Constraint | Definition |
|---|---|
| `id` | INTEGER PRIMARY KEY AUTOINCREMENT |
| `endpoint` | TEXT |
| `node_address` | BLOB |
| `label` | TEXT NOT NULL |
| `source` | TEXT NOT NULL |
| `added_at_unix_ms` | INTEGER NOT NULL |
| `tcp_endpoint` | TEXT |
| `cert_spki_pin` | BLOB |
| `bundle_id` | TEXT |
| `bundle_version` | INTEGER |
| `shared` | INTEGER NOT NULL DEFAULT 0 |

## `one_time_prekeys`

| Column / Constraint | Definition |
|---|---|
| `device_id` | TEXT NOT NULL |
| `prekey_id` | INTEGER NOT NULL |
| `public_key` | TEXT NOT NULL |
| `private_key_protection_scheme` | TEXT NOT NULL |
| `private_key_blob` | TEXT NOT NULL |
| `suite_version` | INTEGER NOT NULL |
| `state` | TEXT NOT NULL DEFAULT 'available' |
| `created_at_unix_ms` | INTEGER NOT NULL |
| `consumed_at_unix_ms` | INTEGER |
| `updated_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | PRIMARY KEY (device_id, prekey_id) |
| _constraint_ | FOREIGN KEY(device_id) REFERENCES local_devices(device_id) |

## `outbound_queue`

| Column / Constraint | Definition |
|---|---|
| `message_id` | TEXT PRIMARY KEY NOT NULL |
| `dialog_key` | TEXT NOT NULL |
| `status` | TEXT NOT NULL |
| `attempt_count` | INTEGER NOT NULL DEFAULT 0 |
| `next_attempt_unix_ms` | INTEGER |
| `last_attempt_unix_ms` | INTEGER |
| `last_failure_kind` | TEXT |
| `last_failure_message` | TEXT |
| `remote_delivered_at_unix_ms` | INTEGER |
| `created_at_unix_ms` | INTEGER NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |

**Added via `ensure_table_column` (additive migrations):**

| Column | Type |
|---|---|
| `profile_id` | TEXT |

## `own_passport`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT PRIMARY KEY NOT NULL |
| `version` | INTEGER NOT NULL |
| `fingerprint` | TEXT NOT NULL |
| `sealed` | BLOB NOT NULL |
| `built_at_unix_ms` | INTEGER NOT NULL |

## `passport_holders`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `account_id` | TEXT NOT NULL |
| `chosen_at_unix_ms` | INTEGER NOT NULL |
| `sent_version` | INTEGER NOT NULL DEFAULT 0 |
| `sent_at_unix_ms` | INTEGER NOT NULL DEFAULT 0 |
| `confirmed_version` | INTEGER NOT NULL DEFAULT 0 |
| `confirmed_at_unix_ms` | INTEGER NOT NULL DEFAULT 0 |
| _constraint_ | PRIMARY KEY (profile_id, account_id) |

## `peer_courier_sets`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `remote_account_id` | TEXT NOT NULL |
| `remote_device_id` | TEXT NOT NULL |
| `publish_json` | TEXT NOT NULL |
| `read_json` | TEXT NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | PRIMARY KEY (profile_id, remote_account_id, remote_device_id) |

## `pending_key_move_devices`

| Column / Constraint | Definition |
|---|---|
| `account_id` | TEXT NOT NULL |
| `device_id` | TEXT NOT NULL |
| `version` | INTEGER NOT NULL |
| _constraint_ | PRIMARY KEY (account_id, device_id) |

## `pending_key_move_seeds`

| Column / Constraint | Definition |
|---|---|
| `account_id` | TEXT PRIMARY KEY NOT NULL |
| `version` | INTEGER NOT NULL |
| `protection_scheme` | TEXT NOT NULL |
| `blob_hex` | TEXT NOT NULL |

## `pending_working_keys`

| Column / Constraint | Definition |
|---|---|
| `account_id` | TEXT PRIMARY KEY NOT NULL |
| `version` | INTEGER NOT NULL |
| `certificate_json` | TEXT NOT NULL |
| `protection_scheme` | TEXT NOT NULL |
| `blob_hex` | TEXT NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |

## `profile_current_device`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT PRIMARY KEY NOT NULL |
| `device_id` | TEXT NOT NULL |
| `created_at_unix_ms` | INTEGER NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | FOREIGN KEY(profile_id) REFERENCES profile_registry(profile_id) |
| _constraint_ | FOREIGN KEY(device_id) REFERENCES local_devices(device_id) |

## `profile_registry`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT PRIMARY KEY NOT NULL |
| `account_id` | TEXT NOT NULL UNIQUE |
| `display_name` | TEXT NOT NULL |
| `public_profile_updated_at_unix_ms` | INTEGER NOT NULL DEFAULT 0 |
| `public_avatar_hash_hex` | TEXT |
| `public_avatar_mime_type` | TEXT |
| `account_representation_version` | INTEGER NOT NULL DEFAULT 2 |
| `profile_state` | TEXT NOT NULL |
| `restored_from_recovery` | INTEGER NOT NULL DEFAULT 0 |
| `created_at_unix_ms` | INTEGER NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | FOREIGN KEY(account_id) REFERENCES local_account(account_id) |

**Added via `ensure_table_column` (additive migrations):**

| Column | Type |
|---|---|
| `public_profile_updated_at_unix_ms` | INTEGER NOT NULL DEFAULT 0 |
| `public_avatar_hash_hex` | TEXT |
| `public_avatar_mime_type` | TEXT |

## `profile_secrets`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT PRIMARY KEY NOT NULL |
| `account_id` | TEXT NOT NULL UNIQUE |
| `recovery_version` | INTEGER NOT NULL |
| `recovery_code` | TEXT NOT NULL |
| `password_enabled` | INTEGER NOT NULL DEFAULT 0 |
| `password_salt_hex` | TEXT |
| `password_verifier_hex` | TEXT |
| `profile_master_key_protection_scheme` | TEXT |
| `profile_master_key_blob_hex` | TEXT |
| `encrypted_profile_secrets_json` | TEXT |
| `restored_from_recovery` | INTEGER NOT NULL DEFAULT 0 |
| `created_at_unix_ms` | INTEGER NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | FOREIGN KEY(profile_id) REFERENCES profile_registry(profile_id) |
| _constraint_ | FOREIGN KEY(account_id) REFERENCES local_account(account_id) |

## `recovered_groups`

| Column / Constraint | Definition |
|---|---|
| `profile_id` | TEXT NOT NULL |
| `group_id` | TEXT NOT NULL |
| `name` | TEXT NOT NULL |
| `role` | TEXT NOT NULL |
| `recovered_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | PRIMARY KEY (profile_id, group_id) |

## `remote_device_observations`

| Column / Constraint | Definition |
|---|---|
| `remote_device_id` | TEXT PRIMARY KEY NOT NULL |
| `remote_account_id` | TEXT NOT NULL |
| `device_public_key` | TEXT NOT NULL |
| `device_label` | TEXT |
| `observed_at_unix_ms` | INTEGER NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |

## `remote_prekey_bundles`

| Column / Constraint | Definition |
|---|---|
| `bundle_key` | TEXT PRIMARY KEY NOT NULL |
| `remote_account_id` | TEXT NOT NULL |
| `remote_device_id` | TEXT NOT NULL |
| `account_display_name` | TEXT |
| `device_label` | TEXT |
| `identity_public_key` | TEXT NOT NULL |
| `device_public_key` | TEXT NOT NULL |
| `signed_prekey_id` | INTEGER NOT NULL |
| `signed_prekey_public_key` | TEXT NOT NULL |
| `signed_prekey_signature_r` | TEXT NOT NULL |
| `signed_prekey_signature_s` | TEXT NOT NULL |
| `one_time_prekey_id` | INTEGER |
| `one_time_prekey_public_key` | TEXT |
| `generated_at_unix_ms` | INTEGER NOT NULL |
| `observed_at_unix_ms` | INTEGER NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |
| `version` | INTEGER NOT NULL |

**Added via `ensure_table_column` (additive migrations):**

| Column | Type |
|---|---|
| `device_certificate_chain_json` | TEXT |
| `identity_signing_public_key` | TEXT |

## `rendezvous_providers`

| Column / Constraint | Definition |
|---|---|
| `id` | INTEGER PRIMARY KEY AUTOINCREMENT |
| `url` | TEXT NOT NULL UNIQUE |
| `source` | TEXT NOT NULL |
| `added_at_unix_ms` | INTEGER NOT NULL |

**Added via `ensure_table_column` (additive migrations):**

| Column | Type |
|---|---|
| `mode` | TEXT NOT NULL DEFAULT 'publish_read' |

## `replay_windows`

| Column / Constraint | Definition |
|---|---|
| `session_address` | TEXT PRIMARY KEY NOT NULL |
| `highest_counter` | INTEGER NOT NULL DEFAULT 0 |
| `recent_tags_json` | TEXT NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |

**Added via `ensure_table_column` (additive migrations):**

| Column | Type |
|---|---|
| `profile_id` | TEXT |

## `sessions`

| Column / Constraint | Definition |
|---|---|
| `session_address` | TEXT PRIMARY KEY NOT NULL |
| `remote_account_id` | TEXT NOT NULL |
| `remote_device_id` | TEXT NOT NULL |
| `local_account_id` | TEXT NOT NULL |
| `local_device_id` | TEXT NOT NULL |
| `local_identity_key` | TEXT NOT NULL |
| `local_device_key` | TEXT NOT NULL |
| `remote_identity_key` | TEXT NOT NULL |
| `remote_device_key` | TEXT NOT NULL |
| `session_key_protection_scheme` | TEXT NOT NULL |
| `session_key_blob` | TEXT NOT NULL |
| `bootstrap_state` | TEXT NOT NULL |
| `send_counter` | INTEGER NOT NULL DEFAULT 0 |
| `receive_counter` | INTEGER NOT NULL DEFAULT 0 |
| `remote_signed_prekey_id` | INTEGER NOT NULL |
| `remote_one_time_prekey_id` | INTEGER |
| `suite_version` | INTEGER NOT NULL |
| `created_at_unix_ms` | INTEGER NOT NULL |
| `updated_at_unix_ms` | INTEGER NOT NULL |

**Added via `ensure_table_column` (additive migrations):**

| Column | Type |
|---|---|
| `profile_id` | TEXT |
| `local_identity_signing_key` | TEXT |
| `remote_identity_signing_key` | TEXT |
| `send_chain_key_protection_scheme` | TEXT |
| `send_chain_key_blob` | TEXT |
| `receive_chain_key_protection_scheme` | TEXT |
| `receive_chain_key_blob` | TEXT |
| `local_ratchet_key_protection_scheme` | TEXT |
| `local_ratchet_key_blob` | TEXT |
| `local_ratchet_public_key` | TEXT |
| `remote_ratchet_public_key` | TEXT |
| `previous_send_chain_length` | INTEGER NOT NULL DEFAULT 0 |
| `needs_send_ratchet` | INTEGER NOT NULL DEFAULT 0 |
| `rendezvous_secret_protection_scheme` | TEXT |
| `rendezvous_secret_blob` | TEXT |
| `revision` | INTEGER NOT NULL DEFAULT 0 |
| `skipped_message_keys_protection_scheme` | TEXT |
| `skipped_message_keys_blob` | TEXT |

## `signed_prekeys`

| Column / Constraint | Definition |
|---|---|
| `device_id` | TEXT NOT NULL |
| `prekey_id` | INTEGER NOT NULL |
| `public_key` | TEXT NOT NULL |
| `private_key_protection_scheme` | TEXT NOT NULL |
| `private_key_blob` | TEXT NOT NULL |
| `signature_r` | TEXT NOT NULL |
| `signature_s` | TEXT NOT NULL |
| `suite_version` | INTEGER NOT NULL |
| `state` | TEXT NOT NULL DEFAULT 'active' |
| `created_at_unix_ms` | INTEGER NOT NULL |
| `expires_at_unix_ms` | INTEGER |
| `updated_at_unix_ms` | INTEGER NOT NULL |
| _constraint_ | PRIMARY KEY (device_id, prekey_id) |
| _constraint_ | FOREIGN KEY(device_id) REFERENCES local_devices(device_id) |

