#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageOwnershipCategory {
    ProfileOwned,
    AccountDeviceScoped,
    DeviceScoped,
    GlobalInstallScoped,
    DiagnosticsCacheOnly,
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
    "group_outbox",
    "group_own_changes",
    "group_message_meta",
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

const GLOBAL_INSTALL_SCOPED_TABLES: &[&str] = &["app_meta"];

const DIAGNOSTICS_CACHE_ONLY_TABLES: &[&str] = &[
    "remote_device_observations",
    "remote_prekey_bundles",
];

pub const PROFILE_DELETE_TABLES: &[&str] = &[
    "local_account",
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
    "group_outbox",
    "group_own_changes",
    "group_message_meta",
    "group_feed_outbox",
    "group_sent_invites",
    "contact_remote_access",
];

pub const PROFILE_WIPE_TABLE_ORDER: &[&str] = &[
    "message_device_receipts",
    "contact_sync_records",
    "contact_sync_device_marks",
    "groups",
    "group_members",
    "group_invites",
    "group_pending_inbound",
    "group_outbox",
    "group_own_changes",
    "group_message_meta",
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
