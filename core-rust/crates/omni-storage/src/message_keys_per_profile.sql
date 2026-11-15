-- Run inside Storage::migrate's transaction after the legacy profile columns are populated.
DROP TRIGGER IF EXISTS message_replies_follow_message;
DROP TRIGGER IF EXISTS message_pins_follow_message;
DROP TRIGGER IF EXISTS message_mentions_follow_message;
DROP TRIGGER IF EXISTS group_message_arrivals_follow_message;
DROP TRIGGER IF EXISTS message_reactions_follow_message;

CREATE TABLE message_replies_scoped (
    message_id TEXT NOT NULL,
    reply_to TEXT NOT NULL,
    profile_id TEXT NOT NULL,
    PRIMARY KEY (profile_id, message_id)
);
INSERT INTO message_replies_scoped (message_id, reply_to, profile_id)
SELECT r.message_id, r.reply_to, m.profile_id
FROM message_replies r JOIN messages m ON m.id = r.message_id WHERE m.profile_id IS NOT NULL;
DROP TABLE message_replies;
ALTER TABLE message_replies_scoped RENAME TO message_replies;

CREATE TABLE message_mentions_scoped (
    message_id TEXT NOT NULL,
    accounts_json TEXT NOT NULL,
    profile_id TEXT NOT NULL,
    PRIMARY KEY (profile_id, message_id)
);
INSERT INTO message_mentions_scoped (message_id, accounts_json, profile_id)
SELECT r.message_id, r.accounts_json, m.profile_id
FROM message_mentions r JOIN messages m ON m.id = r.message_id WHERE m.profile_id IS NOT NULL;
DROP TABLE message_mentions;
ALTER TABLE message_mentions_scoped RENAME TO message_mentions;

CREATE TABLE message_pins_scoped (
    message_id TEXT,
    dialog_key TEXT NOT NULL,
    pinned INTEGER NOT NULL,
    at_unix_ms INTEGER NOT NULL,
    unsent INTEGER NOT NULL DEFAULT 0,
    profile_id TEXT NOT NULL,
    PRIMARY KEY (profile_id, message_id)
);
INSERT INTO message_pins_scoped (message_id, dialog_key, pinned, at_unix_ms, unsent, profile_id)
SELECT r.message_id, r.dialog_key, r.pinned, r.at_unix_ms, r.unsent, m.profile_id
FROM message_pins r JOIN messages m ON m.id = r.message_id WHERE m.profile_id IS NOT NULL;
DROP TABLE message_pins;
ALTER TABLE message_pins_scoped RENAME TO message_pins;
CREATE INDEX IF NOT EXISTS message_pins_by_dialog ON message_pins (dialog_key, pinned);

CREATE TABLE message_reactions_scoped (
    message_id TEXT NOT NULL,
    reactor TEXT NOT NULL,
    mine INTEGER NOT NULL,
    emoji TEXT NOT NULL,
    at_unix_ms INTEGER NOT NULL,
    unsent INTEGER NOT NULL DEFAULT 0,
    profile_id TEXT NOT NULL,
    PRIMARY KEY (profile_id, message_id, reactor)
);
INSERT INTO message_reactions_scoped (message_id, reactor, mine, emoji, at_unix_ms, unsent, profile_id)
SELECT r.message_id, r.reactor, r.mine, r.emoji, r.at_unix_ms, r.unsent, m.profile_id
FROM message_reactions r JOIN messages m ON m.id = r.message_id WHERE m.profile_id IS NOT NULL;
DROP TABLE message_reactions;
ALTER TABLE message_reactions_scoped RENAME TO message_reactions;
CREATE INDEX IF NOT EXISTS message_reactions_unsent ON message_reactions (unsent) WHERE unsent = 1;

CREATE TABLE message_device_receipts_scoped (
    message_id TEXT NOT NULL,
    remote_device_id TEXT NOT NULL,
    delivered_at_unix_ms INTEGER NOT NULL,
    profile_id TEXT NOT NULL,
    PRIMARY KEY (profile_id, message_id, remote_device_id)
);
INSERT INTO message_device_receipts_scoped (message_id, remote_device_id, delivered_at_unix_ms, profile_id)
SELECT message_id, remote_device_id, delivered_at_unix_ms, profile_id FROM message_device_receipts;
DROP TABLE message_device_receipts;
ALTER TABLE message_device_receipts_scoped RENAME TO message_device_receipts;

CREATE TABLE outbound_queue_scoped (
    message_id TEXT NOT NULL,
    dialog_key TEXT NOT NULL,
    status TEXT NOT NULL,
    attempt_count INTEGER NOT NULL DEFAULT 0,
    next_attempt_unix_ms INTEGER,
    last_attempt_unix_ms INTEGER,
    last_failure_kind TEXT,
    last_failure_message TEXT,
    remote_delivered_at_unix_ms INTEGER,
    created_at_unix_ms INTEGER NOT NULL,
    updated_at_unix_ms INTEGER NOT NULL,
    profile_id TEXT,
    PRIMARY KEY (profile_id, message_id)
);
INSERT INTO outbound_queue_scoped (message_id, dialog_key, status, attempt_count, next_attempt_unix_ms, last_attempt_unix_ms, last_failure_kind, last_failure_message, remote_delivered_at_unix_ms, created_at_unix_ms, updated_at_unix_ms, profile_id)
SELECT message_id, dialog_key, status, attempt_count, next_attempt_unix_ms, last_attempt_unix_ms, last_failure_kind, last_failure_message, remote_delivered_at_unix_ms, created_at_unix_ms, updated_at_unix_ms, profile_id FROM outbound_queue;
DROP TABLE outbound_queue;
ALTER TABLE outbound_queue_scoped RENAME TO outbound_queue;
CREATE INDEX IF NOT EXISTS idx_outbound_queue_profile_created ON outbound_queue(profile_id, created_at_unix_ms, message_id);

CREATE TABLE inbound_dedup_scoped (
    message_id TEXT NOT NULL,
    dialog_key TEXT NOT NULL,
    first_seen_unix_ms INTEGER NOT NULL,
    profile_id TEXT,
    PRIMARY KEY (profile_id, message_id)
);
INSERT INTO inbound_dedup_scoped (message_id, dialog_key, first_seen_unix_ms, profile_id)
SELECT message_id, dialog_key, first_seen_unix_ms, profile_id FROM inbound_dedup;
DROP TABLE inbound_dedup;
ALTER TABLE inbound_dedup_scoped RENAME TO inbound_dedup;
CREATE INDEX IF NOT EXISTS idx_inbound_dedup_profile_dialog ON inbound_dedup(profile_id, dialog_key, first_seen_unix_ms);

CREATE TABLE delivery_receipts_scoped (
    message_id TEXT NOT NULL,
    dialog_key TEXT NOT NULL,
    delivered_at_unix_ms INTEGER NOT NULL,
    recorded_at_unix_ms INTEGER NOT NULL,
    profile_id TEXT,
    PRIMARY KEY (profile_id, message_id)
);
INSERT INTO delivery_receipts_scoped (message_id, dialog_key, delivered_at_unix_ms, recorded_at_unix_ms, profile_id)
SELECT message_id, dialog_key, delivered_at_unix_ms, recorded_at_unix_ms, profile_id FROM delivery_receipts;
DROP TABLE delivery_receipts;
ALTER TABLE delivery_receipts_scoped RENAME TO delivery_receipts;
CREATE INDEX IF NOT EXISTS idx_delivery_receipts_profile_dialog ON delivery_receipts(profile_id, dialog_key, recorded_at_unix_ms);

CREATE TABLE message_attachments_scoped (
    message_id TEXT NOT NULL,
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
    profile_id TEXT,
    content_tree_blocks BLOB,
    content_blocks_have BLOB,
    content_encryption TEXT,
    content_key_hex TEXT,
    content_nonce_seed_hex TEXT,
    content_chunk_size_bytes INTEGER,
    active_transfer_id TEXT,
    local_blob_managed INTEGER NOT NULL DEFAULT 1,
    local_source_size_bytes INTEGER,
    local_source_modified_unix_ms INTEGER,
    presentation_json TEXT,
    PRIMARY KEY (profile_id, message_id)
);
INSERT INTO message_attachments_scoped (message_id, dialog_key, file_name, file_size_bytes, mime_type, content_id_hex, state, bytes_transferred, local_blob_path, local_serving_enabled, remote_holder_available, local_saved_path, temp_download_path, last_error, created_at_unix_ms, updated_at_unix_ms, profile_id, content_tree_blocks, content_blocks_have, content_encryption, content_key_hex, content_nonce_seed_hex, content_chunk_size_bytes, active_transfer_id, local_blob_managed, local_source_size_bytes, local_source_modified_unix_ms, presentation_json)
SELECT message_id, dialog_key, file_name, file_size_bytes, mime_type, content_id_hex, state, bytes_transferred, local_blob_path, local_serving_enabled, remote_holder_available, local_saved_path, temp_download_path, last_error, created_at_unix_ms, updated_at_unix_ms, profile_id, content_tree_blocks, content_blocks_have, content_encryption, content_key_hex, content_nonce_seed_hex, content_chunk_size_bytes, active_transfer_id, local_blob_managed, local_source_size_bytes, local_source_modified_unix_ms, presentation_json FROM message_attachments;
DROP TABLE message_attachments;
ALTER TABLE message_attachments_scoped RENAME TO message_attachments;
CREATE INDEX IF NOT EXISTS idx_message_attachments_dialog ON message_attachments(dialog_key, updated_at_unix_ms);
CREATE INDEX IF NOT EXISTS idx_message_attachments_profile_dialog ON message_attachments(profile_id, dialog_key, updated_at_unix_ms);
CREATE INDEX IF NOT EXISTS idx_message_attachments_profile_transfer ON message_attachments(profile_id, active_transfer_id);
CREATE INDEX IF NOT EXISTS idx_message_attachments_pending_download ON message_attachments(profile_id, dialog_key, updated_at_unix_ms) WHERE remote_holder_available = 1 AND state IN ('requested', 'waiting_for_reconnect');

CREATE TABLE message_security_metadata_scoped (
    message_id TEXT NOT NULL,
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
    profile_id TEXT,
    FOREIGN KEY(contact_id) REFERENCES contacts(contact_id),
    PRIMARY KEY (profile_id, message_id)
);
INSERT INTO message_security_metadata_scoped (message_id, dialog_key, contact_id, session_address, remote_account_id, remote_device_id, encryption_state, bootstrap_state, payload_kind, created_at_unix_ms, updated_at_unix_ms, profile_id)
SELECT message_id, dialog_key, contact_id, session_address, remote_account_id, remote_device_id, encryption_state, bootstrap_state, payload_kind, created_at_unix_ms, updated_at_unix_ms, profile_id FROM message_security_metadata;
DROP TABLE message_security_metadata;
ALTER TABLE message_security_metadata_scoped RENAME TO message_security_metadata;
CREATE INDEX IF NOT EXISTS idx_message_security_metadata_profile_dialog ON message_security_metadata(profile_id, dialog_key, created_at_unix_ms DESC);

CREATE TABLE messages_scoped (
    id TEXT NOT NULL,
    body TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at_unix_ms INTEGER NOT NULL,
    direction TEXT NOT NULL DEFAULT 'outgoing',
    updated_at_unix_ms INTEGER NOT NULL DEFAULT 0,
    dialog_key TEXT,
    remote_delivered_at_unix_ms INTEGER,
    copied_from_device_id TEXT,
    profile_id TEXT,
    PRIMARY KEY (profile_id, id)
);
INSERT INTO messages_scoped (id, body, status, created_at_unix_ms, direction, updated_at_unix_ms, dialog_key, remote_delivered_at_unix_ms, copied_from_device_id, profile_id)
SELECT id, body, status, created_at_unix_ms, direction, updated_at_unix_ms, dialog_key, remote_delivered_at_unix_ms, copied_from_device_id, profile_id FROM messages;
DROP TABLE messages;
ALTER TABLE messages_scoped RENAME TO messages;
CREATE INDEX IF NOT EXISTS idx_messages_profile_dialog_created ON messages(profile_id, dialog_key, created_at_unix_ms, id);

CREATE TRIGGER message_children_follow_message AFTER DELETE ON messages BEGIN
    DELETE FROM message_replies WHERE message_id = OLD.id AND profile_id = OLD.profile_id;
    DELETE FROM message_mentions WHERE message_id = OLD.id AND profile_id = OLD.profile_id;
    DELETE FROM message_pins WHERE message_id = OLD.id AND profile_id = OLD.profile_id;
    DELETE FROM message_reactions WHERE message_id = OLD.id AND profile_id = OLD.profile_id;
    DELETE FROM group_message_arrivals WHERE message_id = OLD.id AND profile_id = OLD.profile_id;
END;
