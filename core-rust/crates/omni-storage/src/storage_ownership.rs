#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageOwnershipCategory {
    ProfileOwned,
    AccountDeviceScoped,
    DeviceScoped,
    GlobalInstallScoped,
    DiagnosticsCacheOnly,
    /// Rows of a message, keyed by its id (no profile column): a trigger takes them with the message,
    /// the rest goes with the last profile.
    MessageScoped,
}

// Single canonical ownership map for storage migration and future delete routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageOwnershipEntry {
    pub name: &'static str,
    pub category: StorageOwnershipCategory,
    pub note: &'static str,
}

macro_rules! ownership_entry {
    ($name:literal, $category:ident, $note:literal) => {
        StorageOwnershipEntry {
            name: $name,
            category: StorageOwnershipCategory::$category,
            note: $note,
        }
    };
}

pub const STORAGE_OWNERSHIP_MAP: &[StorageOwnershipEntry] = &[
    ownership_entry!(
        "groups",
        ProfileOwned,
        "groups on MLS: profile-scoped projection / queues (the MLS state itself is wiped separately)"
    ),
    ownership_entry!(
        "group_members",
        ProfileOwned,
        "groups on MLS: profile-scoped projection / queues (the MLS state itself is wiped separately)"
    ),
    ownership_entry!(
        "group_invites",
        ProfileOwned,
        "groups on MLS: profile-scoped projection / queues (the MLS state itself is wiped separately)"
    ),
    ownership_entry!(
        "group_pending_inbound",
        ProfileOwned,
        "groups on MLS: profile-scoped projection / queues (the MLS state itself is wiped separately)"
    ),
    ownership_entry!(
        "group_outbox",
        ProfileOwned,
        "groups on MLS: profile-scoped projection / queues (the MLS state itself is wiped separately)"
    ),
    ownership_entry!(
        "group_own_changes",
        ProfileOwned,
        "groups on MLS: profile-scoped projection / queues (the MLS state itself is wiped separately)"
    ),
    ownership_entry!(
        "group_message_meta",
        ProfileOwned,
        "groups on MLS: profile-scoped projection / queues (the MLS state itself is wiped separately)"
    ),
    ownership_entry!(
        "group_message_arrivals",
        ProfileOwned,
        "groups on MLS: the order group lines came in here (an agent's read place)"
    ),
    ownership_entry!(
        "group_signed_lines",
        ProfileOwned,
        "profile-scoped (a profile_id column): goes with its profile"
    ),
    ownership_entry!(
        "group_member_profiles",
        ProfileOwned,
        "profile-scoped (a profile_id column): goes with its profile"
    ),
    ownership_entry!(
        "group_branches",
        ProfileOwned,
        "profile-scoped (a profile_id column): goes with its profile"
    ),
    ownership_entry!(
        "group_pending_welcomes",
        ProfileOwned,
        "profile-scoped (a profile_id column): goes with its profile"
    ),
    ownership_entry!(
        "group_readd_requests",
        ProfileOwned,
        "profile-scoped (a profile_id column): goes with its profile"
    ),
    ownership_entry!(
        "recovered_groups",
        ProfileOwned,
        "profile-scoped (a profile_id column): goes with its profile"
    ),
    ownership_entry!(
        "history_imports",
        ProfileOwned,
        "profile-scoped (a profile_id column): goes with its profile"
    ),
    ownership_entry!(
        "chat_history_checks",
        ProfileOwned,
        "profile-scoped (a profile_id column): goes with its profile"
    ),
    ownership_entry!(
        "own_passport",
        ProfileOwned,
        "profile-scoped (a profile_id column): goes with its profile"
    ),
    ownership_entry!(
        "passport_holders",
        ProfileOwned,
        "profile-scoped (a profile_id column): goes with its profile"
    ),
    ownership_entry!(
        "held_passports",
        ProfileOwned,
        "profile-scoped (a profile_id column): goes with its profile"
    ),
    ownership_entry!(
        "peer_courier_sets",
        ProfileOwned,
        "profile-scoped (a profile_id column): goes with its profile"
    ),
    ownership_entry!(
        "allowed_pair_couriers",
        ProfileOwned,
        "profile-scoped (a profile_id column): goes with its profile"
    ),
    ownership_entry!(
        "dialog_read_marks",
        ProfileOwned,
        "how far each chat of the profile is read"
    ),
    ownership_entry!(
        "message_replies",
        ProfileOwned,
        "what a message answers: goes with its message (trigger)"
    ),
    ownership_entry!(
        "message_mentions",
        ProfileOwned,
        "who a message names: goes with its message (trigger)"
    ),
    ownership_entry!(
        "message_pins",
        ProfileOwned,
        "pins: go with their message (trigger)"
    ),
    ownership_entry!(
        "message_reactions",
        ProfileOwned,
        "reactions: go with their message (trigger)"
    ),
    ownership_entry!(
        "message_deletions",
        ProfileOwned,
        "tombstones of the profile's deleted lines"
    ),
    ownership_entry!(
        "message_delete_requests",
        ProfileOwned,
        "deletions asked for the profile's lines not here yet"
    ),
    ownership_entry!(
        "message_actions_mark",
        MessageScoped,
        "when a message action last changed: goes with the last profile"
    ),
    ownership_entry!(
        "account_working_keys",
        AccountDeviceScoped,
        "newest working key seen per account"
    ),
    ownership_entry!(
        "account_key_moves",
        AccountDeviceScoped,
        "working key moves per account"
    ),
    ownership_entry!(
        "pending_key_move_devices",
        AccountDeviceScoped,
        "devices a key move still waits for"
    ),
    ownership_entry!(
        "pending_key_move_seeds",
        AccountDeviceScoped,
        "a key move held until the password is at hand"
    ),
    ownership_entry!(
        "pending_working_keys",
        AccountDeviceScoped,
        "a working key held until the password is at hand"
    ),
    ownership_entry!(
        "device_revocations",
        AccountDeviceScoped,
        "revoked devices of accounts, as received"
    ),
    ownership_entry!(
        "contact_block_periods",
        ProfileOwned,
        "when a contact was blocked, and until when"
    ),
    ownership_entry!(
        "group_invitee_welcomes",
        ProfileOwned,
        "the Welcome made for an invitee, until it is heard in the group"
    ),
    ownership_entry!(
        "inbound_bootstrap_marks",
        ProfileOwned,
        "which remote devices bootstrapped a session to this profile"
    ),
    ownership_entry!(
        "durable_frames",
        DeviceScoped,
        "durable mesh feed frames of this node"
    ),
    ownership_entry!(
        "durable_feed_seq",
        DeviceScoped,
        "durable mesh feed counters of this node"
    ),
    ownership_entry!(
        "mesh_dialog_route",
        DeviceScoped,
        "mesh node -> dialog of this node"
    ),
    ownership_entry!(
        "mesh_peer_book",
        DeviceScoped,
        "mesh peers this node met"
    ),
    ownership_entry!(
        "mesh_bootstrap_invites",
        DeviceScoped,
        "mesh invites this node imported"
    ),
    ownership_entry!(
        "mls_storage_key",
        DeviceScoped,
        "the MLS storage key from when there was one per database: the first profile to ask takes it"
    ),
    ownership_entry!(
        "mls_storage_keys",
        ProfileOwned,
        "the key the profile's MLS state is sealed with (its rows go by it: retain_mls_rows)"
    ),
    ownership_entry!(
        "network_nodes",
        GlobalInstallScoped,
        "relays the user added: the install keeps them"
    ),
    ownership_entry!(
        "rendezvous_providers",
        GlobalInstallScoped,
        "couriers the user chose: the install keeps them"
    ),
    ownership_entry!(
        "workspace_repos",
        ProfileOwned,
        "workspaces: the group's repositories here (published bare repo, bound working repo)"
    ),
    ownership_entry!(
        "workspace_member_refs",
        ProfileOwned,
        "workspaces: the branches each member announced"
    ),
    ownership_entry!(
        "workspace_main_rules",
        ProfileOwned,
        "workspaces: each code project's rule for its shared main"
    ),
    ownership_entry!(
        "workspace_approvals",
        ProfileOwned,
        "workspaces: commits members approved for the shared main"
    ),
    ownership_entry!("workspace_repo_files", ProfileOwned, "file workspace repository modes"),
    ownership_entry!("workspace_file_events", ProfileOwned, "authenticated file workspace roles, policy and reviews"),
    ownership_entry!("workspace_file_local", ProfileOwned, "local folders and explicit download intent"),
    ownership_entry!(
        "group_feed_outbox",
        ProfileOwned,
        "groups on MLS: profile-scoped projection / queues (the MLS state itself is wiped separately)"
    ),
    ownership_entry!(
        "group_sent_invites",
        ProfileOwned,
        "groups on MLS: profile-scoped projection / queues (the MLS state itself is wiped separately)"
    ),
    ownership_entry!(
        "contact_remote_access",
        ProfileOwned,
        "standing remote access per contact, both directions (remote-access-for-contacts.md)"
    ),
    ownership_entry!(
        "local_account",
        AccountDeviceScoped,
        "installation-local account root and FK anchor for the selected profile"
    ),
    ownership_entry!(
        "profile_registry",
        ProfileOwned,
        "canonical local profile registry"
    ),
    ownership_entry!(
        "profile_secrets",
        ProfileOwned,
        "profile-scoped unlock and secret metadata foundation"
    ),
    ownership_entry!(
        "profile_current_device",
        ProfileOwned,
        "profile-scoped current-device pointer foundation"
    ),
    ownership_entry!(
        "messages",
        ProfileOwned,
        "profile-scoped message history"
    ),
    ownership_entry!(
        "message_attachments",
        ProfileOwned,
        "profile-scoped attachment transfer metadata and local blob references"
    ),
    ownership_entry!(
        "contacts",
        ProfileOwned,
        "profile-scoped contact graph"
    ),
    ownership_entry!(
        "contact_dialog_links",
        ProfileOwned,
        "profile-scoped contact to dialog bridge"
    ),
    ownership_entry!(
        "contact_verifications",
        ProfileOwned,
        "profile-scoped trust verification state"
    ),
    ownership_entry!(
        "contact_identity_observations",
        ProfileOwned,
        "profile-scoped observed identity snapshots"
    ),
    ownership_entry!(
        "contact_presence",
        ProfileOwned,
        "profile-scoped contact presence cache"
    ),
    ownership_entry!(
        "identity_keys",
        AccountDeviceScoped,
        "account-scoped identity key material"
    ),
    ownership_entry!(
        "message_device_receipts",
        ProfileOwned,
        "which devices of a contact confirmed a message"
    ),
    ownership_entry!(
        "contact_sync_records",
        ProfileOwned,
        "contacts as state shared by the account's devices, removals as tombstones"
    ),
    ownership_entry!(
        "contact_sync_device_marks",
        ProfileOwned,
        "how far each of our devices has applied this device's contact changes"
    ),
    ownership_entry!(
        "account_keys",
        AccountDeviceScoped,
        "account key chain: public master key, portable working key"
    ),
    ownership_entry!(
        "account_devices",
        AccountDeviceScoped,
        "certificates of the account's devices"
    ),
    ownership_entry!(
        "sessions",
        ProfileOwned,
        "profile-scoped session records"
    ),
    ownership_entry!(
        "local_devices",
        DeviceScoped,
        "installation-local current device inventory"
    ),
    ownership_entry!(
        "device_keys",
        DeviceScoped,
        "installation-local crypto key material"
    ),
    ownership_entry!(
        "signed_prekeys",
        DeviceScoped,
        "installation-local signed prekeys"
    ),
    ownership_entry!(
        "one_time_prekeys",
        DeviceScoped,
        "installation-local one-time prekeys"
    ),
    ownership_entry!(
        "replay_windows",
        ProfileOwned,
        "profile-scoped anti-replay state"
    ),
    ownership_entry!(
        "app_meta",
        GlobalInstallScoped,
        "app-level metadata and process-wide flags"
    ),
    ownership_entry!(
        "outbound_queue",
        ProfileOwned,
        "profile-scoped queue state for local delivery"
    ),
    ownership_entry!(
        "inbound_dedup",
        ProfileOwned,
        "profile-scoped inbound duplicate suppression cache"
    ),
    ownership_entry!(
        "delivery_receipts",
        ProfileOwned,
        "profile-scoped delivery receipt cache"
    ),
    ownership_entry!(
        "remote_device_observations",
        DiagnosticsCacheOnly,
        "derived remote device observation cache"
    ),
    ownership_entry!(
        "remote_prekey_bundles",
        DiagnosticsCacheOnly,
        "derived remote prekey bundle cache"
    ),
    ownership_entry!(
        "message_security_metadata",
        ProfileOwned,
        "profile-scoped per-message security projection"
    ),
    ownership_entry!(
        "call_sessions",
        ProfileOwned,
        "profile-scoped call session metadata and outcomes"
    ),
    ownership_entry!(
        "call_participants",
        ProfileOwned,
        "profile-scoped call participant snapshots"
    ),
];

const PROFILE_OWNED_TABLES: &[&str] = &[
    "profile_registry",
    "message_replies",
    "message_mentions",
    "message_pins",
    "message_reactions",
    "message_device_receipts",
    "contact_sync_records",
    "contact_sync_device_marks",
    "profile_secrets",
    "profile_current_device",
    "messages",
    "message_attachments",
    "contacts",
    "contact_dialog_links",
    "contact_verifications",
    "contact_identity_observations",
    "contact_presence",
    "sessions",
    "replay_windows",
    "inbound_bootstrap_marks",
    "group_invitee_welcomes",
    "contact_block_periods",
    "outbound_queue",
    "inbound_dedup",
    "delivery_receipts",
    "message_security_metadata",
    "call_sessions",
    "call_participants",
    "groups",
    "group_members",
    "group_invites",
    "group_pending_inbound",
    "mls_storage_keys",
    "group_outbox",
    "group_own_changes",
    "group_message_meta",
    "group_message_arrivals",
    "group_signed_lines",
    "group_member_profiles",
    "group_branches",
    "group_pending_welcomes",
    "group_readd_requests",
    "recovered_groups",
    "history_imports",
    "chat_history_checks",
    "own_passport",
    "passport_holders",
    "held_passports",
    "peer_courier_sets",
    "allowed_pair_couriers",
    "dialog_read_marks",
    "message_deletions",
    "message_delete_requests",
    "workspace_repos",
    "workspace_member_refs",
    "workspace_main_rules",
    "workspace_approvals",
    "workspace_repo_files",
    "workspace_file_events",
    "workspace_file_local",
    "group_feed_outbox",
    "group_sent_invites",
    "contact_remote_access",
];

const ACCOUNT_DEVICE_SCOPED_TABLES: &[&str] = &[
    "local_account",
    "identity_keys",
    "account_keys",
    "account_devices",
];

const DEVICE_SCOPED_TABLES: &[&str] = &[
    "local_devices",
    "device_keys",
    "signed_prekeys",
    "one_time_prekeys",
];

const GLOBAL_INSTALL_SCOPED_TABLES: &[&str] = &["app_meta", "network_nodes", "rendezvous_providers"];

const DIAGNOSTICS_CACHE_ONLY_TABLES: &[&str] = &[
    "remote_device_observations",
    "remote_prekey_bundles",
];

pub const PROFILE_DELETE_TABLES: &[&str] = &[
    "local_account",
    "message_replies",
    "message_mentions",
    "message_pins",
    "message_reactions",
    "message_device_receipts",
    "contact_sync_records",
    "contact_sync_device_marks",
    "profile_registry",
    "profile_secrets",
    "profile_current_device",
    "messages",
    "message_attachments",
    "contacts",
    "contact_dialog_links",
    "contact_verifications",
    "contact_identity_observations",
    "contact_presence",
    "sessions",
    "replay_windows",
    "outbound_queue",
    "inbound_dedup",
    "delivery_receipts",
    "message_security_metadata",
    "call_participants",
    "call_sessions",
    "local_devices",
    "device_keys",
    "signed_prekeys",
    "one_time_prekeys",
    "identity_keys",
    "account_keys",
    "account_devices",
    "groups",
    "group_members",
    "group_invites",
    "group_pending_inbound",
    "mls_storage_keys",
    "group_outbox",
    "group_own_changes",
    "group_message_meta",
    "group_message_arrivals",
    "group_signed_lines",
    "group_member_profiles",
    "group_branches",
    "group_pending_welcomes",
    "group_readd_requests",
    "recovered_groups",
    "history_imports",
    "chat_history_checks",
    "own_passport",
    "passport_holders",
    "held_passports",
    "peer_courier_sets",
    "allowed_pair_couriers",
    "dialog_read_marks",
    "message_deletions",
    "message_delete_requests",
    "workspace_repos",
    "workspace_member_refs",
    "workspace_main_rules",
    "workspace_approvals",
    "workspace_repo_files",
    "workspace_file_events",
    "workspace_file_local",
    "group_feed_outbox",
    "group_sent_invites",
    "contact_remote_access",
];

pub const PROFILE_WIPE_TABLE_ORDER: &[&str] = &[
    "message_replies",
    "message_mentions",
    "message_pins",
    "message_reactions",
    "message_device_receipts",
    "contact_sync_records",
    "contact_sync_device_marks",
    "groups",
    "group_members",
    "group_invites",
    "group_pending_inbound",
    "mls_storage_keys",
    "group_outbox",
    "group_own_changes",
    "group_message_meta",
    "group_message_arrivals",
    "group_signed_lines",
    "group_member_profiles",
    "group_branches",
    "group_pending_welcomes",
    "group_readd_requests",
    "recovered_groups",
    "history_imports",
    "chat_history_checks",
    "own_passport",
    "passport_holders",
    "held_passports",
    "peer_courier_sets",
    "allowed_pair_couriers",
    "dialog_read_marks",
    "message_deletions",
    "message_delete_requests",
    "workspace_repos",
    "workspace_member_refs",
    "workspace_main_rules",
    "workspace_approvals",
    "workspace_repo_files",
    "workspace_file_events",
    "workspace_file_local",
    "group_feed_outbox",
    "group_sent_invites",
    "contact_remote_access",
    "message_attachments",
    "messages",
    "message_security_metadata",
    "call_participants",
    "call_sessions",
    "contact_dialog_links",
    "contact_verifications",
    "contact_identity_observations",
    "contact_presence",
    "contacts",
    "outbound_queue",
    "inbound_dedup",
    "delivery_receipts",
    "sessions",
    "replay_windows",
    "inbound_bootstrap_marks",
    "group_invitee_welcomes",
    "contact_block_periods",
    "one_time_prekeys",
    "signed_prekeys",
    "device_keys",
    "identity_keys",
    "local_devices",
];

pub fn ownership_category_for_table(name: &str) -> Option<StorageOwnershipCategory> {
    STORAGE_OWNERSHIP_MAP
        .iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.category)
}

pub fn profile_owned_tables() -> &'static [&'static str] {
    PROFILE_OWNED_TABLES
}

pub fn account_device_scoped_tables() -> &'static [&'static str] {
    ACCOUNT_DEVICE_SCOPED_TABLES
}

pub fn device_scoped_tables() -> &'static [&'static str] {
    DEVICE_SCOPED_TABLES
}

pub fn global_install_scoped_tables() -> &'static [&'static str] {
    GLOBAL_INSTALL_SCOPED_TABLES
}

pub fn diagnostics_cache_only_tables() -> &'static [&'static str] {
    DIAGNOSTICS_CACHE_ONLY_TABLES
}

pub fn profile_delete_tables() -> &'static [&'static str] {
    PROFILE_DELETE_TABLES
}
